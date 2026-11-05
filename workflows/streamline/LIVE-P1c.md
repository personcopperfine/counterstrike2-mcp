# Live check of live-fix C (2026-10-26, streamline/agent-ux at b724e13, bridge m3.16)

I ran one Hammer tools session (`TOOL=hammer ADDON=mcp_test bridge/runtime.sh up`, no `runtime.sh
open`) and drove it through the worktree's `target/debug/hammer-mcp` (`--project-root
<project root>`, no other flags). I only tested and recorded; no code was changed. The
raw evidence is under `target/live-p1c/`:
- `01-open_map.txt` and `03-console.txt` hold every reply in order;
- `scripts/` holds the drivers, and `bin/run.py` is the live-p1b driver (it reads extra server args
  from `HMCP_ARGS`);
- `console-session.log` is the game's console.log.

## Versions

- hammer-mcp / client-mcp: `cargo build -j2` debug at b724e13 (`build.log`), built with the game down.
- Bridge: m3.16 as installed in LIVE-P1b (unchanged, no reinstall); `install-rs.sh ensure` said "all
  artefacts current" (`runtime-up.txt`). Game build 2000933.

## Checks

| # | Check | Verdict | Evidence |
|---|---|---|---|
| 1a | fresh session: `session_status` shows hammer.dll `not_loaded` | PASS | `01-open_map.txt` (1st reply) |
| 1b | `open_map {path:"maps/p1_test.vmap"}`, no set_addon: `openedVia:"vtm"` plus a document | PASS | `01-open_map.txt` (2nd reply) |
| 1c | `session_status` afterwards: hammer.dll loaded, `map.open` advertised | PASS | `01-open_map.txt` (3rd reply) |
| 2 | second `open_map` of the same map gives the existing/active reply | PASS | `01-open_map.txt` (4th reply) |
| 3a | `console_exec` of a 510-byte single command runs | PASS | `03-console.txt` |
| 3b | a 511-byte command is refused locally | PASS | `03-console.txt` |
| 3c | a 129-byte command name is refused locally (128 runs) | PASS | `03-console.txt` |
| 4 | provoke `OpenUnconfirmed`, then `OpenPending` | SKIP | see below |
| 5 | `runtime.sh down` | PASS | `runtime-down.txt` |

### 1. First-map open_map over vtm
- Before the open: `bridgeVersion m3.16`, 33 methods, `modules["hammer.dll"] {state:"not_loaded"}`,
  `addon {name:"mcp_test", source:"flag", pinned:null}`.
- `open_map {path:"maps/p1_test.vmap"}` (no `set_addon` call) answered in **2.61 s** with:
  - `opened:true, openedVia:"vtm", existing:false`;
  - `engineAddon:"mcp_test", engineAddonSource:"container"`;
  - `post {command:"open_asset maps/p1_test.vmap", delivered:true, confirmed:true, retried:false}`,
    whose vtmsend output shows `posted record seq=0`;
  - the note "the bridge did not advertise map.open yet … later open_map calls use map.open";
  - `document {id:"d1", epoch:1, active:true, dirty:false, editable:true, documentAddon:"mcp_test",
    path:"z:/app/content/csgo_addons/mcp_test/maps/p1_test.vmap"}`.
- console.log shows the `Handling VToolMessage` block with `_message = "open_asset"`
  (`console-session.log` line 516).
- No `HammerNotLoaded` / `AddonCheckFailed` refusal came before the post, so the review fix that maps
  `doc.active` -32017 to `HammerNotLoaded` works live.
- `session_status` afterwards: 59 methods including `map.open`, hammer.dll `verified`,
  `addon.source:"document"`.

### 2. Second open_map
The second open answered in 0.10 s with `existing:true, active:true, opened:false, epoch:1`. The note
says the file is already open and is the active document, and suggests reload_map.

### 3. Console limits (after `run_map {path:"maps/p1_test.vmap"}`, state ok)
- **`echo <z…>`, 510 bytes:** sent on the bridge route, `endedBy:"marker"`, `markerSeen:true`, no
  `deliveryWarning`. echo prints don't reach console.log on the bridge route (the tool says so in a
  note), so I also checked with `hostname`.
- **`hostname "<499 y>"`, 510 bytes:** ran. `cvar_get hostname` read back all 499 characters. No
  `deliveryWarning`.
- **511 bytes (both the echo and the hostname form):** refused locally in 0.00 s. The reply was
  `InvalidArguments` (`-32602`, `retrySafe:true`) with the message "a `;`-separated command is 511
  bytes; the engine drops any single command over 510 bytes … Nothing was sent".
- **Command name `p1c_n…`, 128 bytes:** sent. The engine answered `Unknown command: p1c_n…`
  (`endedBy:"marker"`).
- **Command name, 129 bytes:** refused locally with "a command name is 129 bytes; the engine drops a
  command whose name is over 128 bytes ("Console command too long.") … Nothing was sent".
- At the end, `hostname "cs2-docker"` was restored and read back.

### 4. OpenUnconfirmed / OpenPending: SKIP
- To time out, the vtm open has to take longer than the call budget, and the budget is at least 5 s
  (`BudgetTooLow` below that).
- The largest map in mcp_test is p1_test (362 KB), and its first open, Hammer start included, took
  2.6 s. The other mcp_test maps are smaller.
- So a budget just over 5 s would not time out on any of them.
- The other way to force a timeout is a map that parses locally but that Hammer rejects. That raises
  the blocking "Error opening file" modal in the tools session, which is not safe.
- ti_convoy is off limits.
- `OpenPending` needs an unconfirmed first post, so it could not be reached either.
- Both stay mock-tested only.

### 5. Teardown
`runtime.sh down` → "container cs2tools removed". `podman ps` was empty afterwards.

## Bugs

None found in the checked items.

## Part 2: acceptance session setup (harness, not counted against the agent)

**Addon `accept_p1`.**
- The project has no tool or script that creates addons. hammer-mcp has `set_addon` but no
  create_addon. runtime.sh only passes `-addon`.
- The project's own addon (ti_convoy) is just `game/csgo_addons/<name>/addoninfo.txt` plus a content
  directory, with nothing copied from addon_template. Hammer writes the rest (`_bakeresourcecache`,
  `tools_asset_info.bin`, ...) itself.
- addon_template's addoninfo has `IsTemplate 1`, and its content includes the `xxx_mapname_xxx` map,
  which is why I did not copy it.
- Under `<project root>/workshop/run/app/` I did only this:
  ```
  mkdir -p content/csgo_addons/accept_p1/maps game/csgo_addons/accept_p1
  printf '"AddonInfo"\n{\n\t"IsTemplate"\t"0"\n\t"IsPlayable"\t"1"\n}\n' > game/csgo_addons/accept_p1/addoninfo.txt
  ```
- The addoninfo is the same as mcp_test's. No `"maps"` key, because there are no maps.
- `content/.../maps/` is empty. Nothing else exists, and nothing under ti_convoy was touched.

**Persistent driver: `<repo>/target/orch/mcp_session.py`.**
- mcp_drive.py starts one server per invocation, so set_addon, a pending open and in-process job
  state are lost between invocations.
- mcp_session.py runs one server behind a unix socket. It uses a background daemon that does the MCP
  `initialize` / `notifications/initialized` handshake.
  ```
  mcp_session.py start <bin> [server args...]     # returns after initialize + tools/list answer (90 s deadline)
  mcp_session.py call <tool> ['<json args>'|@file.json]   # prints the result JSON; exit 0 ok, 1 isError/JSON-RPC error, 2 driver failure
  mcp_session.py list [--full]
  mcp_session.py status
  mcp_session.py stop                             # closes the server's stdin, waits, removes the socket
  ```
- Server args pass through unchanged, as in mcp_drive. mcp_drive uses no budget flags beyond
  `--project-root`, so the server defaults apply: `--tool-budget-ms 45000`, `--deadline-ms 30000`.
- The daemon's reply wait is `--tool-budget-ms` + 60 s, which only catches a hung server.
- State lives in `$MCP_SESSION_DIR`, default `target/orch/mcp_session/`:
  - `sock`;
  - `daemon.log`;
  - `server.stderr.log`;
  - `transcript.jsonl`, every request and reply with a timestamp;
  - `images/`, where image content is saved as files and the printed JSON shows the path.
- Overlapping calls from several shells are matched by JSON-RPC id.
- Tested against the real server (`target/live-p1c/sess-test/`, game down):
  - start, status, list (91 tools) and session_status all worked;
  - after `set_addon mcp_test`, the next `session_status` showed `pinned:"mcp_test"`, so state
    persists;
  - an unknown tool exits 1, and bad JSON exits 2;
  - stop leaves no process and no socket. A first version of stop served one more connection after
    stop; that was fixed with `shutdown()`.

**Session (left RUNNING).**
- `TOOL=hammer ADDON=accept_p1 bridge/runtime.sh up`: container `cs2tools`, label
  `cs2mcp.addon=accept_p1` (`runtime-up-accept_p1.txt`).
- The driver was started with:
  `mcp_session.py start <repo>/.claude/worktrees/streamline/target/debug/hammer-mcp --project-root <project root> --addon accept_p1`
- `session_status` (`accept_p1-session_status.json`):
  - `bridge.connected:true`, m3.16, build 2000933;
  - `addon {name:"accept_p1", source:"flag"}`;
  - 33 methods, hammer.dll `not_loaded`, no document.
- This server holds the bridge connection (one client). Another hammer-mcp that calls a bridge tool
  gets `Busy`.
- To tear down: `mcp_session.py stop`, then `bridge/runtime.sh down`.

No commit was made.
