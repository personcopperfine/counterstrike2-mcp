# client-mcp — agent guide

Rust **stdio MCP server** that drives the CS2 **game client** for testing and play: in-game unit
tests (the engine's own `CTestScriptMgr` runner), multiplayer sessions, CS script / VScript test
maps, demo recording and demo->video, and AI playtesting (the model drives the player through a
usercmd-level input hook and sees screenshots). Shared goals with `../hammer-mcp/`: signature-backed
game-update porting, vision + OCR, headless runs, docker runtime first / local install later.
This file is self-contained: it repeats the conventions shared with `../hammer-mcp/` on purpose.
The **code** those conventions describe is shared through the `cs2-mcp-common` crate.

Status (2026-10-11): **bootstrapped** — stdio transport, bridge client, mock bridge,
`session_status` (flavour + tool-group availability) and a guarded `console_exec` work (workflow 00
done); `game_tick`, `checkpoint`, `input_set`, `input_clear` are implemented against the mock bridge
(not yet exercised live); `console_tail`/`console_wait` work on the host file. Workflow 01
(docker runtime: `launch_session`, `wait_session`, `stop_session`) is **implemented, not yet
live-verified**: argv golden tests and a fake podman cover it, no real container has been started.
`screenshot` and `observe` (workflows 08/09) are implemented on the Vulkan capture layer
(`cs2-vk-capture`): frames come from the game's swapchain, never an X grab. Tests use a fake layer
(a thread that answers `request.json`) and a fake podman. The layer itself was captured live on
LinuxClient (20/20 frames, ~120 ms each); this server's launch-with-layer and screenshot path is not
live-verified yet, and the Wine path is **unverified**. The other tools are gated stubs pointing at
their workflow.

## Where things are

This repo is its own project root (override with `--project-root` / `CS2_MCP_PROJECT_ROOT`):

```
<repo>/                     THIS repo = the default project root
├── crates/cs2-bridge/      the bridge DLL/.so (PROTOCOL.md: its wire contract)
├── crates/cs2-bridge-tools/ `cargo bridge`: pins, build, install/ensure, the Wine tools runtime
├── crates/cs2-mcp-common/  shared crate: bridge client, tool registry, rmcp adapter, runtime, sigs, capture, mock
├── hammer-mcp/             workshop-tools server (own AGENT.md)
├── client-mcp/             this server
└── workshop/               submodule: the workshop tools image source, published as ghcr.io/personcopperfine/cs2-workshop
    └── run/app             Windows CS2 + tools + Proton prefix (gitignored runtime state; mounted at /app)
```

Paths are written from the repo root as `crates/cs2-bridge/...`, `crates/cs2-mcp-common/...`,
`workshop/...`. Override with `--sigs` / `--project-root`.
Citations of the form `../idabins/...`, `../harness/...`, `../tests/...`, `../cs2-gym/...`,
`../.github/skills/...` point into the maintainer's private research checkout (the directory this
repo used to live in). They are not published; they record where a fact was established.

Required reading (skim; cite, do not re-derive):

| Topic | Where |
|---|---|
| Page format, evidence tags `[V] [I] [?] [S] [D]` | `../idabins/docs/FORMAT.md` |
| Addresses, game update, bridge/runtime rules | `../idabins/docs/WORKFLOW.md` §9, §10, §11 |
| Bridge protocol, methods, error codes (§5) | `crates/cs2-bridge/PROTOCOL.md` |
| Bridge DLL (this repo) | `crates/cs2-bridge/README.md`, `crates/cs2-bridge/PROTOCOL.md` (the wire contract), `crates/cs2-bridge/src/registry.rs` (the method table) + `ops/`, `cargo bridge pins`, `cargo bridge runtime` |
| Signature DB format | `../idabins/docs/data/signatures-format.md` |
| Test scripts (`CTestScriptMgr`) | `../idabins/worknotes/vtest/README.md`, `vtest-scratch.md`, `docs/interfaces/ITestScriptMgr.md`, `worknotes/vtest/run_vtest.sh`, `run_gametest.sh`, `gametest/` |
| Input hook (`input.*`) | `../idabins/docs/flows/client-input-pipeline.md`, `worknotes/inputhook-scratch.md`, `worknotes/vtest/input_hook_test.py` |
| Demo -> video | `../idabins/worknotes/movie-scratch.md`, `worknotes/vtest/suites/movie_demo.txt`, `check_movie_demo.py` |
| Freecam world render (no Hammer) | `../idabins/docs/flows/map-preview-freecam.md`, `worknotes/worldpreview-scratch.md` |
| Python bridge client / POC server (the contract we ported) | `../idabins/worknotes/mcpserver/bridge_client.py`, `server.py`, `mock_bridge.py`, `README.md` |
| Linux headless client image (`ghcr.io/personcopperfine/cs2-docker`) | [`client/Dockerfile`](https://github.com/personcopperfine/cs2-docker/blob/main/client/Dockerfile), [`client/entrypoint.sh`](https://github.com/personcopperfine/cs2-docker/blob/main/client/entrypoint.sh), `../harness/run-cs2.sh`, `boot.sh`, `x11shim.c`, `xdump.c`, `xkey.c`, `../tests/README.md`, `../.github/skills/cs2-lavapipe/SKILL.md` |
| Windows runtime under Wine | `workshop/Dockerfile`, `workshop/entrypoint.sh`, `../idabins/worknotes/wine-tools-bringup.md` |
| RL environment tie-in | `../cs2-gym/docs/DESIGN.md` |

## Purpose and scope

In scope (priority order):
1. **Unit tests in the real game**: stage + run `-testscript` suites, checkpoints as barriers,
   judge from `console.log`/PNGs/demos; many tests per session via the parked harness + bridge
   (`test.checkpoint`, `input.*`, `console.exec`, `game.tick`) as in `run_gametest.sh`.
2. **Multiplayer**: dedicated server + client(s); bots; LAN connect. Several CS2 instances may run
   at once; GPU memory runs out at about 3 instances at 1080p (`--max-sessions`, default 2).
3. **CS script & VScript testing**: load a test map with `point_script` (JS from TS, `v8system`)
   or VScript (`vscript.dll`), run, collect output.
4. **Demo recording -> video**: SourceTV `.dem`, then the engine's own `startmovie` webm path.
5. **AI playtest**: observe (`game_state`/`radar`/`weapons`/`sounds_drain` from inside the game, plus screenshot + OCR) -> act (`input.set`, `select_weapon`) loop;
   action/trace format compatible with `cs2-gym`.
6. Shared: sig-backed build pinning, vision/OCR, headless, docker-first runtime.

Non-goals:
- Keypress/X11 input injection as the primary input path (the hook is usercmd-level). `xkey`-style
  input is only a fallback for sessions without a bridge and must be flagged as such.
- A reimplementation of the game loop for training speed: that is `cs2-gym`; this server is the
  oracle/real-game side.
- VAC/anti-cheat evasion. Everything runs with `-insecure`/gbe emulator against local servers only.
- Patching Valve code; the bridge only vptr-swaps empty slots and calls documented entry points.

## Architecture

```
MCP client (stdio) ──rmcp──▶ cs2-mcp-common::mcp     transport adapter; owns stdout; the only rmcp importer
                              ▼
                   cs2-mcp-common::tools              registry: name → schema, mutability, bridge methods, handler
      ┌──────────┬────────────┼────────────┬─────────────┬──────────────┬─────────────┐
      ▼          ▼            ▼            ▼             ▼              ▼             ▼
 common::    common::     client::     client::      client::     common::       client::
 runtime     bridge       console      testscript    input        capture        video
 docker /    hello,       console.log  suites +      input.set    screenshot /   tv_record,
 local       deadlines,   tail/wait;   fragments;    /clear;      X grab /       playdemo
             caps, gate,  vtmsend;     checkpoints   angles       world.render;  --startmovie
             taxonomy     refusal      (test.*)                   ocr (tesseract)
             guard                client::playtest: observe→act loop, gym-compatible trace
                                  common::sigs: build pin + doctor (no addresses)
```

- **Shared crate.** `cs2-mcp-common` holds the bridge client, registry (`ToolSpec`, `ToolError`,
  `Budget`, `ToolState`), the rmcp adapter (`serve_stdio`), `status` (the bridge half of
  `session_status`), `runtime`, `sigs`, `capture`, `config` (`CommonConfig`, flattened into this
  binary's `ClientConfig` which adds `--flavour` and `--console-log`), and behind the `mock` feature
  the Rust mock bridge + stdio test driver. This crate owns client-specific code only: `tools/`,
  `state`, `console` (routes + the refusal guard), `testscript`, `input`, `video`, `playtest`.

### Two session flavours (important)

| Flavour | What runs | Headless today | Bridge | Proven for |
|---|---|---|---|---|
| **WineTools** | `cs2mcp_launch.exe -tools -insecure -testscript ...` under Proton in `ghcr.io/personcopperfine/cs2-docker`, tree `workshop/run/app` at `/app` (the launcher starts `cs2.exe` suspended and injects the Metamod loader DLL) | **No** (needs host X; Xvfb dies: BAD_EDID -> DXVK `EXCEPTION_INT_DIVIDE_BY_ZERO`) | yes (`cs2_bridge.dll` tool plugin) | vtest suites, gametest runner, input hook, movie recording, world.render |
| **LinuxClient** | native `cs2.sh -steam` in `ghcr.io/personcopperfine/cs2-docker` (gbe, lavapipe fork or `--gpus all`), `harness/run-cs2.sh` | **Yes** (in-container `Xvfb :99 -fbdir`) | no (no tools on Linux; no bridge) | boot to menu, framebuffer snapshots, `tests/cs2/t30` |
| NativeWindows | `cs2.exe` on the Windows host this server runs on, started by `cs2mcp-native up` (`crates/cs2-native`); the server attaches only (`%LOCALAPPDATA%\cs2mcpbridge\session.json`, `--console-log`, `--app-root` from `cs2mcp-native mcp-config`) | n/a (the user's desktop) | yes (the same bridge, tool plugin and/or Metamod) | bridge tools live on the test VM 2026-11-04 (`workflows/native-launcher-report.md`); no podman groups (`session`, `multiplayer`), no capture layer |
| LinuxDedicated | `cs2 -dedicated` via `docker/linux-dedicated.sh`, no X (strings in `libengine2.so`) | n/a | yes (`server_up` passes `+sv_hibernate_when_empty 0` by default: an empty server hibernates otherwise, no GameFrame, the Metamod-hosted bridge never ticks; `hibernate: true` opts out) plus RCON | **implemented, not yet live-verified** (workflows/05); the bridge tick on an empty server was live-verified 2026-10-20 with the convar at 0 |

The bridge-backed tools (`input_*`, `checkpoint`, `console_exec`, `game_tick`, `render_world`) require
WineTools until a Linux-side equivalent exists (open question). `--flavour` (default `auto`: WineTools
when the bridge answers hello, NativeWindows when that happens on a Windows host, else `unknown`) and `session_status.toolGroups` say which tools are
available and why not.

### Relation to the POC and the bridge

The Python pieces are the contract: `bridge_client.py` (fully ported to
`crates/cs2-mcp-common/src/bridge/client.rs`), `mock_bridge.py` (ported to `crates/cs2-mcp-common/src/mock.rs`),
`run_vtest.sh` + `gametest/runner.py` (staging, parking, reset modes `soft|round|hard`,
`gt_begin_/gt_pass_/gt_fail_<test>` checkpoints, JUnit), `input_hook_test.py`, `check_*.py` judges.
Bridge m2.8 methods used here (all **implemented**, `[V runtime]`): `input.set/clear/status/uninstall`,
`test.checkpoint`, `test.active`, `console.exec` (only in `-testscript` sessions; never during a level
load), `game.tick`, `world.render`; `game.saveRestore` exists but `load` is unusable as a reset and it
is on the forbidden list.
`input.set` matches buttons by name and a mask of exactly `1 << i` (`in_find_bit`); `moveup` / `movedown` carry
mask -1 (IN_ALL), so they cannot be matched and are not reachable through `input.set` / `input_set`.

**The bridge source lives in this repo** (`crates/cs2-bridge`). Methods this server will need and that **do not
exist yet** — server-side hooks, any Linux-side hook — are added in `crates/cs2-bridge` (a handler in `src/ops/<group>`), pinned from `sigs/signatures`
through `cargo bridge pins`, and documented in `crates/cs2-bridge/PROTOCOL.md` (`crates/cs2-bridge/README.md`
"Adding a method"; request list in `research/bridge-wishlist.md`, workflow 02).

## Build, run, test

```sh
cd <repo>
cargo clippy --workspace --all-targets   # must be warning-free
cargo test  --workspace                  # common contract tests + server tests + stdio e2e (no game)
cargo build -p client-mcp
RUST_LOG=info cargo run -p client-mcp -- --runtime docker --flavour auto   # project root: this checkout
```

Live session today (until workflow 01): `../idabins/worknotes/vtest/run_vtest.sh sandbox` (parks at
`Test_Checkpoint vtest_release`), then `../idabins/worknotes/vtest/load_bridge.sh`; needs host X and
`XAUTHORITY`; mind the session cap (`CS2_MCP_MAX_SESSIONS`). podman needs `XDG_RUNTIME_DIR=<project root>/.podman/xdg`.

### Configuration (flags = env)

Common (`cs2-mcp-common::config::CommonConfig`): `--runtime docker|local` (`CS2_MCP_RUNTIME`),
`--discovery` (`CS2_MCP_DISCOVERY`), `--pfx` (`CS2_MCP_PFX`), `--project-root` (`CS2_MCP_PROJECT_ROOT`),
`--sigs` (`CS2_MCP_SIGS`), `--path-map` (`CS2_MCP_PATH_MAP`), `--expect-build`
(`CS2_MCP_EXPECT_BUILD`), `--deadline-ms 30000`, `--tool-budget-ms 45000`, `--blocked-age-ms 10000`,
`--no-blocked-gate`, `--launch`, `--max-sessions 2` (`CS2_MCP_MAX_SESSIONS`, min 1), `--podman podman`
(`CS2_MCP_PODMAN`; tests point it at a fake), `--image` (`CS2_MCP_IMAGE`, default
`ghcr.io/personcopperfine/cs2-docker:latest`; a local build of `cs2-docker/client` goes here). `--vk-layer-dir` (`CS2_MCP_VK_LAYER_DIR`, default
`<repo>/target/vklayer`): the capture layer's directory (`libcs2_vk_capture.so` +
`VkLayer_cs2mcp_capture.json`, built and copied by `docker/build-vk-layer.sh`). When both files are
there, every launch (linux-client, wine-tools, testscript runs) mounts it read-only at `/cs2mcp/vk`,
sets `VK_ADD_LAYER_PATH=/cs2mcp/vk`, `VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture` and
`CS2MCP_CAPTURE_DIR=/work/capture`, and creates `<artifacts>/capture` (0700). `launch_session
{capture: false}` opts a session out. `session_status` reports `runtime.vkLayer` (valid, missing).
Both: `--app-root` (`CS2_MCP_APP_ROOT`, default `<project-root>/workshop/run/app`): the game tree
(`game/`, `content/`); a native install passes its install root. On a Windows host the path map has
no prefix entries (a bridge-reported `C:\...` path is a host path), `--discovery` defaults to
`%LOCALAPPDATA%\cs2mcpbridge\session.json`, `--sigs` falls back to `<exe dir>/sigs`, and
`launch_session` refuses (the session is `cs2mcp-native`'s).
Client-only: `--flavour auto|wine-tools|wine-client|linux-client|linux-dedicated|native-windows`
(`CS2_MCP_FLAVOUR`), `--console-log <host path>` (`CS2_MCP_CONSOLE_LOG`). Console tools pick the file in this order:
flag; the `session` argument (a `cs2mcp-*` container); the one running session; else
`<app-root>/game/csgo/console.log` (several running sessions without `session`
is an error). The result says `source`.

## Conventions (binding)

1. **No addresses in Rust.** Never write an RVA, vtable slot or struct offset in any crate of this
   repo. They live in `sigs/signatures/<module>.json` and reach the game only through the bridge
   DLL (`cargo bridge pins` -> `crates/cs2-bridge/src/generated/pins.rs`, fail-closed `-32017`).
   `common::sigs` reads the DB by entry name for build pinning and `doctor`. New symbol = new
   signature entry + bridge method in `crates/cs2-bridge` first.
2. **stdout is the protocol.** Logs via `tracing` to stderr. Never `println!`. The stdio e2e test
   asserts every stdout line is a JSON-RPC frame.
3. **Error taxonomy** (`common::bridge::errors`, `common::tools::ToolError`): `{type, name, code,
   wireName, message, data, retrySafe, outcomeUnknown}`; `retrySafe:false` = may have executed, verify
   instead of retrying. Wire codes -32001..-32022 map 1:1. `code` is a JSON-RPC-style code when one applies
   (bridge wire codes; `-32602` for many host-side argument/path refusals); many host-side refusals
   (`ForbiddenCommand`, `ToolBudgetExceeded`, ...) have `code: null`. It is not unique per type, so
   clients switch on `type`/`name`, never on `code`. Server-side classes here:
   `ForbiddenCommand` (console guard), `ConsoleLogMissing` (console.log absent: data `{path, hint}`;
   retrySafe), `WaitTimeout` (console_wait: data `{regex, path?, fileExists?, waitedMs, timeoutMs,
   requestedTimeoutMs, clampedToBudget, lastLines}`; retrySafe), `ConsoleLogIo` (other console.log I/O;
   retrySafe), `ToolBudgetExceeded` (less than 500 ms of call budget left), later `SessionNotRunning`,
   `AnotherCs2Running`, `DisplayUnavailable`; the launch ensure's `InstallBusy` (exit 4, retrySafe),
   `InstallFailed` (other non-zero exit, retrySafe), `InstallTimeout` (retrySafe: false, data
   `{timeoutSecs, hint}`: the tree may be half-repaired) and `InstallToolMissing` (a configuration
   error: the path, retrySafe: false — retrying without fixing the configuration fails the same way).
4. **Deadlines** on every bridge request (explicit `deadlineMs`, never 0) from the per-call `Budget`;
   timeout classification as the POC (`QueueTimeout` safe, `OutcomeUnknown`/`ConnectionLost` not).
5. **Liveness gate.** `control.ping` before non-`control.*` methods; stale `mainThreadAgeMs` ->
   `HammerBlocked` (name kept from the protocol; in a client session it means a modal/stalled main
   thread). **Never send `console.exec` during a level load** (crashed the server in testing).
6. **Capability gating** per tool (registry); `NotImplemented{data.missing}` without sending anything;
   a dead bridge is `BridgeNotRunning`, never a missing capability.
7. **Safety policies.** `console_exec` only in `-testscript` or `-tools` sessions (the bridge enforces it; the tool
   surfaces the bridge's `-32011`); it refuses `quit`/`exit`/`disconnect`/`Test_ExitProcess`/`crash`/`maps`
   (`cs2_mcp_common::console::refusal`, shared with hammer-mcp, applied to every `;`/newline segment) — use `stop_session`; scripts
   end with `quit` not `Test_ExitProcess` (flushes `console.log`); `input_clear` + `input.uninstall` on
   session end; never exposed (registry panics): `control.shutdown`, `console.command`, `game.saveRestore`.
8. **Session limits.** Several CS2 containers may run at once. Two limits, both checked by
   `launch_session` under an in-process gate and a cross-process lock (`.podman/cs2mcp-launch.lock`):
   - **Global cap**: at most `--max-sessions` (`CS2_MCP_MAX_SESSIONS`, default 2 = one Wine + one
     Linux) running `cs2*` containers, foreign ones included (GPU memory: about 3 fit at 1080p).
     Refused with `TooManySessions` (data: running containers, max, hint).
   - **One per kind per shared mount**: one Wine session (wine-tools or wine-client; both mount
     the app tree) per app dir (`/app`), one LinuxClient
     session and one LinuxDedicated session per `cs2-home` volume (a client and a server may run
     together: the shared preamble is flock-serialised). Refused with `KindBusy` (data: kind, container, shared).
     An identical request while its session runs returns that session (`reused: true`).
   - TODO before lifting the per-kind rule: isolate per-session writes (overlay the game dir and
     the home volume) so two sessions of one kind cannot share state.
   Every launch first runs `cs2-bridge-tools install --platform <wine|linux> ensure` for its tree
   (Wine app dir / `cs2-home`) under the same gate and lock: read-only while the tree is current,
   repaired when something is missing/stale, and refused with `InstallBusy` (naming the containers)
   while a running container mounts a tree that needs repair; a reused session does not run it. The
   program is `--install-tool` / `CS2_MCP_INSTALL_TOOL` when given, else the `cs2-bridge-tools`
   binary next to this client-mcp (`cargo build --release -p cs2-bridge-tools` builds it; a missing
   one fails the launch with `InstallToolMissing`, never a skipped ensure). It installs the
   `target/bridge-rs` build of the checkout it was built from, so a worktree build installs **that
   worktree's** bridge into the shared tree (ensure re-installs whichever build launched last);
   backups and the Metamod build stay in the canonical checkout's `target/` (`CS2_MCP_DIR`, default
   the repo's main checkout; the runtime does not pass it, it passes `CS2_MCP_PROJECT_ROOT` = its
   own project root). A checkout with no `target/bridge-rs` build and nothing installed fails the
   launch with the tool's message.
   The ensure is bounded at 120 s and runs in its own process group (a timeout SIGTERMs, waits 5 s,
   then SIGKILLs the group while the launch lock is held; a cancelled call may leave a partial
   install that the next ensure repairs).
   **Launching**: Wine sessions start `cs2mcp_launch.exe`, not `cs2.exe` - the launcher
   (`crates/mm-launch`) starts the game suspended, injects the Metamod loader DLL and waits, so a
   failure is exit 3 with `[cs2mcp-launch] FAIL: ...` instead of a Metamod-less session; Linux
   sessions prepend `csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so` to `LD_PRELOAD`
   on the `cs2.sh` command only and refuse to launch without it. Neither edits `gameinfo.gi`;
   real Windows / Steam-launched retail CS2 is not a target (crates/metamod/README.md
   "Launching").
   Every session loads Metamod:Source with the cs2-mcp bridge as one plugin (third-party Metamod
   plugins may sit next to it), and Linux client and wine-client sessions pass `-insecure` (VAC
   off, no official/secure servers; local and LAN servers are unaffected). A wine-client session
   runs the bridge as a plain Metamod plugin: its `ISource2Server::GameFrame` tick starts the bridge
   listener once a map runs (`args: ["+map", "<map>"]`, or a later map load), not at the menu, so
   the readiness ladder still ends at the menu marker in the app tree's console.log (live
   2026-10-20: input, sound wrap, saveRestore and `metamod.plugins` answer over RPC after `+map`).
   Every session container is named `cs2mcp-<kind>-<short id>`. Containers run without `--rm`, so
   logs survive the game; `stop_session` stops them (`podman stop -t 0`, never killed by us), waits
   for podman to report the exit (a slow stop is `pending: true`; call `stop_session` again), then
   removes them (`podman rm`). `stop_session` and `wait_session` only act on `cs2mcp-*` names.
9. **Evidence first.** A test passes on `console.log` markers / files / pixels the judge can read, not
   on the absence of errors; `TESTSCRIPT: CheckPoint -> '<name>'` lines (with `testscript_debug 1`)
   are the timeline. A checkpoint that arrives during `Test_Wait` is dropped — use barriers.
10. **Ask before driving the desktop** (host X keypresses/focus).
11. **Game update** = `workflows/game-update.md` + `cargo bridge pins`/`cargo bridge build`; here only `BuildMismatch`
    until the DB and bridge move.
12. **Waits.** Two kinds, two shapes. *Job waits* (`wait_session`, `test_wait`) wait for a job to
    reach a stage or finish and return a status report: when the budget or timeout ends the wait, the
    report comes back with `pending: true` and `next` (the call to continue with). *Predicate waits*
    (`console_wait`) wait for a condition on a line and return a `WaitTimeout` error when it is not met.
    Both clamp to the call budget minus a reserve.
13. **Refuse before writing.** A tool that stages files or launches a container runs the runtime's
    `preflight` (cap, one session per kind per mount) first, so a refused call leaves nothing behind.
    A busy error names the owning run (`data.runId`) when this server's run owns the container.
14. **Agent-readable results.** Annotations, `next`, `didYouMean`/`useInstead` and tool aliases come
    from `cs2-mcp-common` (see hammer-mcp/AGENT.md convention 13). Legacy string-valued `next` fields in
    this crate (`runtime_tools`, `multiplayer`, `testscript`, `game_state`) predate the array shape and
    are converted in WP2.2; do not add new ones.

## Layout

```
client-mcp/
├── AGENT.md, README.md, Cargo.toml       lib (client_mcp) + bin (client-mcp)
├── src/
│   ├── lib.rs, main.rs    build_server(); clap (ClientConfig = CommonConfig + flavour/console-log)
│   ├── state.rs           ClientConfig, Flavour, ClientState (runtime + session table)
│   ├── tools/             client.rs (tool set + TOOL_GROUPS), session.rs (session_status, console_exec),
│   │                      runtime_tools.rs (launch_session, stop_session), bridge_tools.rs, console.rs
│   ├── console/           refusal guard (tested); console.log tail/wait (`log.rs`, `tools/console.rs`)
│   ├── testscript/        mod.rs: suite render + stage (assets/vtest/); judge.rs: judges (move_jump preset);
│   │                      run.rs: run table, background follower, collection (run_testscript, test_status, test_wait)
│   ├── tools/capture.rs   screenshot + observe (capture layer frames, console part, checkpoint events, screen guess)
│   ├── input/             input.set shapes (skeleton)
│   ├── video/             MovieSpec (playdemo --startmovie options), demo-to-video suite, judge (ffprobe optional); tools/video.rs: demo_record_start/stop, demo_to_video
│   └── playtest/          observe→act loop (skeleton)
├── tests/                 server.rs (mock-backed), stdio.rs (real binary over stdio)
├── assets/vtest/          POC suites + fragments, byte-identical (include_str!); README lists sources
├── fixtures/              testscript/ console.log excerpts (judge tests); the bridge recording lives in crates/cs2-mcp-common/fixtures/
├── research/              dev tooling (Python ok): judges, recorders, gym adapters
├── docker/                linux-client.sh + linux-dedicated.sh (headless Linux launch scripts, mounted read-only), lib-preamble.sh (shared, flock-serialised preamble), build-vk-layer.sh (builds cs2-vk-capture into the layer dir); our image layer (OCR, headless display, helpers; not built yet)
└── workflows/             00..10
```

## Workflows

| # | File | Outcome |
|---|---|---|
| 00 | `workflows/00-bootstrap.md` | **done**: rmcp stdio server, bridge client, mock bridge, `session_status`, `console_exec` guard, tests |
| 01 | `workflows/01-docker-runtime.md` | `docker` backend: `launch_session`, `wait_session`, `stop_session` (podman; both flavours; limits; readiness ladder). **Implemented, not yet live-verified.** Headless Wine and our image layer open. |
| 02 | `workflows/02-sigs.md` | build pinning, `doctor`, update drill, new-method procedure (in `crates/cs2-bridge`) |
| 03 | `workflows/03-console-convars.md` | console.log tail/wait, command routes, convar snapshots |
| 04 | `workflows/04-unit-tests.md` | `run_testscript`, `test_status`, `test_wait`, judges: **implemented, not yet live-verified**; parked harness + JUnit open (step 4) |
| 05 | `workflows/05-multiplayer.md` | dedicated server (`server_up`), `client_connect`, `server_console_exec` (RCON): **implemented, not yet live-verified**; bots over RCON |
| 06 | `workflows/06-cs-script-vscript.md` | point_script (JS/TS) and VScript test maps: survey done; `script_run` validates and answers NotImplementedYet (both kinds blocked, see the workflow's Status) |
| 07 | `workflows/07-demo-record-video.md` | SourceTV record, `--startmovie webm`, artifacts |
| 08 | `workflows/08-playtest-ai.md` | observe/act loop, gym-compatible traces; observations from `game_state`/`radar`/`sounds_drain` (11) |
| 09 | `workflows/09-vision-ocr.md` | screenshots as image blocks, OCR, HUD reads |
| 10 | `workflows/10-local-install.md` | `local` runtime backend |
| 11 | `workflows/11-game-state.md` | in-game state via the bridge: `game_state`, `weapons`, `radar` (+ overview projection), `select_weapon`, `sounds_drain`/`sounds_status` (bridge m2.9 `game.local/weapons/radar/selectWeapon`, `sound.*`); bridge m2.10 adds `game_rules` (`game.rules`), `entity_find` (`game.entities`), `game_wait` `roundLive`/`roundRestarted`/`ammo`, and player armor/helmet/money/ducked/moveType. **Live-verified 2026-10-11** for the m2.9 tools (35/38 probe checks; `research/game-state.md`, `research/game_state_probe.py`); m2.10 not yet live-probed |

## Behaviour notes (live-fix B)

- **`BridgeBusy`.** Every bridge-backed tool waits for the bridge lock at most 10 s (clamped to the
  call budget); a longer-running holder (`input_sequence`, `game_wait`, a `setpos` wait) makes a
  concurrent call answer the retry-safe `BridgeBusy` (nothing was sent) instead of queuing.
- **`cvar_get` redaction by name.** `rcon_password`, `sv_password`, `sv_setsteamaccount` and any name
  containing `password` or `token` come back `redacted:true`, `redactedBy:"name"`, value and default
  null (`cs2_mcp_common::console::redact_cvar_reply`, shared with hammer-mcp). `found:false` entries
  are never marked.
- **`input_clear`** answers `held:[]`; what the bridge listed (it replies before the release is
  applied) is in `releasedHeld`.

## Shared play-test code (WP1.6)

`game_state`, `input_sequence` and the `game_wait` engine are implemented in
`crates/cs2-mcp-common/src/play/` (`state`, `sequence`, `input`, `wait`) and hammer-mcp registers the
same names over them (plus `play_test`); this crate keeps the tool wrappers (the `ClientState`
lock, the `game_wait` schema, the registration) and its names, schemas and output unchanged. A
change to the argument rules, the tick pacing or the output shape of those tools is made there and
reaches both servers; run both servers' tests. `game_rules` (play follow-ups) is shared the same way (`play::state::rules`, also registered in hammer-mcp; output unchanged). `entity_find` (live-fix D) is shared the same way (`play::entities`: the `class`/`name`/`max`
validation, the `game.entities` request, the schema); client-mcp's tool keeps its name, schema,
fields and `source` note, and hammer-mcp registers it plus `play_test`'s `wait_entity` /
`expect_entity` steps over the same call. `play_test` is hammer-mcp only for now (it needs the
tools session's console.log tail; the client side would use `ConsoleLog`, see
`workflows/streamline/FIXME.md` "WP1.6"). `run_testscript` stays what it was: a NEW `-testscript`
session, not an in-session driver.

## Tool notes (moved from descriptions, WP2.2)

`tools/list` is paid for in every agent's context, so descriptions say what a tool does, what it is
not for, units, and the traps that cost a call. Everything else lives here. The size guard is
`tests/tool_list_size.rs` (32000 B total, 700 chars per description, 120 per property). The old
wording is in git history (`git log -p -- client-mcp/src/tools/client.rs`, commit "client-mcp
WP2.2").

- **launch_session.** Every launch first runs `cs2-bridge-tools install ... ensure` for its tree: a current
  tree is untouched, a missing/stale one is repaired, and the launch is refused (`InstallBusy`,
  naming the containers) while a running container mounts a tree that needs repair. The install
  phase can exceed the call budget on a first install or right after a game update; the call then
  returns the install outcome as it stands (`InstallFailed`/`InstallTimeout`/`InstallBusy`). The
  program is the `cs2-bridge-tools` next to this client-mcp (or `--install-tool`), and it installs
  the bridge build of the checkout it was built from, so a worktree build installs that worktree's
  bridge into the shared tree (ensure re-installs whichever build launched last); a checkout without
  a `target/bridge-rs` build and nothing installed fails with the tool's message. Every session loads Metamod:Source with the
  cs2-mcp bridge as one plugin; third-party Metamod plugins can sit next to it. linux-client and
  wine-client run with `-insecure` (VAC off, no official servers; local/LAN servers unaffected).
  wine-client: Metamod loads the bridge as a plugin; bridge RPC only once a map runs (the GameFrame
  tick starts the listener; none at the menu); `MenuReached` there is the main-menu marker line in
  the app tree's console.log (no channel). Refuses at `--max-sessions` running cs2* containers and
  when a session of that kind already uses the same mount (`KindBusy`). An identical running
  request returns that session (`reused: true`) without re-running ensure. Waits at most
  `min(timeoutMs, call budget - 2 s)`; `pending: true` means continue with `wait_session`. Artifacts
  in `<project-root>/artifacts/<container>-<ts>/`. "Not verified live yet" was the state at m2.
  `connect`: linux-client does NOT work (the client joins its own loopback listen server; open
  question, workflows/05); wine-client takes it as a plain `+connect <addr>` engine arg at launch
  (no bridge console.exec there); wine-tools refuses it, use `client_connect`. `args` on
  wine-client refuses tools/testscript/addon in any spelling (`-tools`, `+TestScript`, a bare
  addon, any case): they would silently make the session another kind. `map` (wine-tools): [A-Za-z0-9_-]
  segments joined by `/`, no `--`, no `..`, at most 128 bytes. Staging writes the per-map harness
  `<app>/vtest/harness-<map with each / as ->.txt`, then launches it (loads the map, spawns the
  player, parks: `Parked`). With `addon` the map loads as `map_workshop <addon> <map>` only when it
  is the addon's own (`<app>/game/csgo_addons/<addon>/maps/<map>.vpk`), else as `map`; the file is
  then `harness-<addon>--<map>.txt`. `MapLoadFailed` (not retry-safe; session left running) when
  the engine refuses the map or `game.local` never shows it in game on that map within ~10 s.
  Result gains `map, mapCommand, harness, stagedPath, mapVerified` (false with `mapNote` for bridge
  false, a reused session, or no answer). `testscript`: file must exist at `<app>/vtest/<name>.txt`
  (`SuiteNotStaged` otherwise); with `map`, leave it out or pass `harness`. `bridge`: false stops
  the wait at the channel (`MenuReached`), `BridgeHello` is reported unchecked. `capture` needs
  `CS2_MCP_VK_LAYER_DIR` to hold a built layer. `waitFor` defaults: MenuReached (linux-client,
  wine-client), Parked with `testscript`, else BridgeHello (wine-tools; MenuReached with bridge
  false); ServerLive for server_up. wine-tools MenuReached = VToolMessage channel up (vtmsend),
  BridgeHello = cs2_bridge loaded and `control.hello` answers. `headless` must be true for
  linux-client, false for the wine kinds. `gpus` is `--gpus all`.
- **wait_session.** Reads only the container state and console.log; no bridge.
- **stop_session.** `podman stop -t 0`, wait for the exit, then `podman rm` without `-f`
  (containers run without `--rm`, so logs stay until this). Before removal, best-effort
  `input.clear` and `input.uninstall` over a connected bridge (`inputCleared`, `inputUninstalled`).
  Default: the session this server launched last; refuses non-`cs2mcp-*` containers. `timing`: ms
  since the stop began for `stopIssuedMs, exitedMs, rmIssuedMs, removedMs` (null when not reached),
  `totalMs`, `initialState` (unknown when podman did not answer; the stop is issued anyway). A
  stop still running when the budget ends is `pending: true` (the stop is not reissued). When
  podman's storage stalls, cleanup and removal can trail the game's death by minutes.
- **console_tail / console_wait.** File pick: `--console-log` if set; else the `session` arg; else
  the one running cs2mcp-* session; else `<project>/workshop/run/app/game/csgo/console.log`; the
  result says `path` and `source`. console_wait follows truncation and rotation; a `since` path
  that differs from the resolved file wins (source cursor), an inode change restarts at 0
  (`cursorReset`). `timeoutMs` is capped to the call budget minus 500 ms, 0 only with
  `fromStart`/`since`. Neither needs a bridge.
- **console_exec.** Gated on `console.exec`; `-testscript` or `-tools` sessions only. `consoleCursor`
  comes from the console.log of the `session` arg (or console_tail's pick). `confirm` queues a
  marker right after the command and waits (bounded by the call budget, `confirmTimeoutMs`
  default 5000, `clampedToBudget`) for `Unknown command: <marker>` in console.log: the engine
  processed the command in order, not that it succeeded. Returns `confirmed, confirmMs,
  clampedToBudget, warnings` (the `Unknown command:` lines before the marker).
- **game_tick / checkpoint.** bridge `game.tick` / `test.checkpoint`. Checkpoints arriving during
  `Test_Wait` are dropped. **cvar_get**: bridge `cvar.get`; protected cvars and secret names
  (`rcon_password`, `sv_password`, `*password*`, `*token*`) come back `redacted: true`, no value.
- **input_set.** bridge `input.set`, a usercmd-level hook. `turn {dyaw, dpitch}` is resolved
  against the active override via `input.status`. Release + tap of the same held button in one
  call re-presses it (bridge m3.15+). The shared field wording lives in `cs2-mcp-common`
  (`play/input.rs`, used by hammer-mcp too); client-mcp shortens it with `tools::retext`.
- **input_sequence.** Hold counted from the first `game.local` reading after the step's
  `input.set` (`startTick`); pacing is by real samples (15 ms). Budget: about 100 ms per step +
  ticks/64 s + 1.5 s, checked before anything is sent (`ToolBudgetExceeded`, `maxTotalTicks`).
  The closing `input.clear` may overrun the call budget by up to 1 s; `inputCleared` is true,
  `"failed"` or false. An error after the first `input.set` carries `stepsCompleted`. Returns
  `steps` (startTick, endTick, origin, viewAngles), `totals` (ticks, distance, waitedMs), `start`,
  `final`.
- **run_testscript / test_status / test_wait.** Launches a wine-tools session with `-testscript`
  (no `-testmode`). A background task follows console.log (checkpoint timeline), stops at
  container exit or timeout, collects console.log, `vtest_*` PNGs and media into `artifactsDir`
  and judges with the optional preset (`move_jump`); without it the run passes on exit code 0.
  Not verified live at m2. `demo` additionally requires a `vtest_*.dem` over 10000 bytes.
- **demo_record_start / demo_record_stop.** Gated on `console.exec`. Start sends `tv_delay 0;
  tv_record <name>` and waits up to 2 s for "Recording SourceTV replay from": `recording: true`,
  `RecordingNotStarted` (retry-safe), or `"unconfirmed"` with no resolvable console.log. Stop sends
  `tv_stoprecord`, waits up to 5 s for "Completed SourceTV demo" (`DemoNotCompleted`), returns
  `path, bytes, confirmedBy`; a `.dem` older than the call is `DemoStale`; without a console.log it
  falls back to the file size settling.
- **demo_to_video.** Stages `movie_<name>_<ms>` (`playdemo <demo> --startmovie <name> <format>
  framerate N no_unique_dir`, wait for `DemoPlaybackFinished`, quit) and runs it through
  run_testscript under wine-tools on every flavour (the Linux frame-dump runner is a TODO,
  workflows/07). The judge checks the checkpoints, the recorder's Finishing-movie count, the file
  (>= 10000 B, EBML magic), with ffprobe the stream/packet/decoded-frame count (within 2%) and
  `r_frame_rate`, and a contact sheet with ffmpeg. Absent tools are in `skipped`.
- **server_up.** `cs2 -dedicated`, `--network=host`, no X; the `cs2-home` volume is shared with
  clients under a flock. Every launch first runs `cs2-bridge-tools install --platform linux ensure`
  on the shared tree (stale trees repaired unless a running container mounts them: `InstallBusy`).
  RCON password random if absent, saved to `server-session.json` (mode 0600). Full log is stdout,
  saved to `server.log`. Markers seen live: `SetServerState ss_active`, socket opened,
  hibernating. `hibernate` false passes `+sv_hibernate_when_empty 0` before `args` because an
  empty hibernating server runs no GameFrame, so the Metamod-hosted bridge never starts its
  listener until a client joins; a `+sv_hibernate_when_empty` in `args` wins. `bots` are added
  via `bot_add_ct/_t` over RCON once live (`applied: false` while pending). `map` is
  `[A-Za-z0-9_]{1,64}`, `rconPassword` `[A-Za-z0-9_.-]{1,64}`. Returns `container, port,
  rconCandidates, rconPassword`.
- **client_connect.** wine-tools via bridge `console.exec`; linux-client has no command route
  (NotImplemented, alternative `launch_session {connect}`).
- **server_console_exec.** Password/port from `server-session.json` (works from any MCP process);
  `rconPassword`/`port` override. Refuses the console_exec list plus `rcon_password`,
  `sv_shutdown` and aliases that run them; commands over 4086 bytes.
- **script_run.** Kinds: `cs_script` (compiled addon map scripts) and `vscript` (retail CS2 runs
  VScript on CNopVM: `NotImplementedYet`; inline `source`/`path` also, compiling JS needs
  resourcecompiler). `entry` is `{target, input}` = `ent_fire <target> RunScriptInput <input>`
  (the point_script entity is not client-visible) or `{command}`; forbidden commands are refused
  before launch; an engine "Unknown command" fails the run. The judged output starts just before
  the entry fires. `expect` regexes match the WHOLE logged line, which starts with a
  `MM/DD HH:MM:SS ` timestamp; `^` anchors at the timestamp; considered lines are `[cs_script] `
  lines and script error lines (every line with `anyLine`; the confirmation marker never). The run
  passes when every expect matches in order and no script error line appears. `map` defaults to
  the addon's name (loaded with `map_workshop`); `MapLoadFailed` when game.local does not report
  it. A wait the call budget cuts short is pending (`passed` null).
- **game_test.** Steps run in-process through the same handlers, each with a slice of the call's
  budget; only a session launch the budget cuts short is `pending: true` with `next`. 30 s per-step
  cap (`StepTimeout`, `TestTimeout`); a tool's own error keeps its type. `BudgetTooSmall
  {neededMs}` when setup/steps/cleanup cannot fit, or when the budget runs out during the steps.
  Teardown runs `input_clear` first. Markers `gt_begin_<name>` / `gt_pass_<name>` / `gt_fail_<name>`
  go through `checkpoint` (best effort). `demo: true` records a SourceTV demo around the steps
  (`demo.path, demo.ticks`); `verbose: false` drops the results of passing steps (failed ones carry
  `result`, trimmed to 8 KiB). `script_run` is not a step: it launches its own session. `approx`
  tolerance default 0.001. Returns `{name, passed, pending, steps:[{phase, label, tool, ok, ms,
  error?, assertions:[{path, op, wanted, got, ok}]}], durationMs, ticks:{start,end}, demo?}`.
  Example spec: `assets/gametests/zoo_smoke.json`; a minimal step:
  `{"label":"walk","tool":"input_sequence","args":{"steps":[{"press":["forward"],"holdTicks":64}]},
  "expect":[{"path":"totals.distance","op":"ge","value":100}]}`.
- **game_test_results.** `format: junit` is one testsuite per spec (properties `ticks`,
  `demo.path`, `next`), one testcase per step (`phase.label`), failure elements with the failing
  assertions, `skipped` for steps that did not run. Reads the store only.
- **render_world.** bridge `world.render` (Wine tools runtime), offscreen freecam, not the player's
  view. Writes a PNG on the host; the inline image is downscaled above 1920x1080;
  `width*height <= 33554432`.
- **game_state / weapons / radar / game_rules.** Bridge `game.local` (live-verified 2026-10-11),
  `game.weapons`, `game.radar` (projected onto `resource/overviews/<map>.txt`), `game.rules`
  (schema-resolved through the cs_gamerules proxy; null fields listed in `schemaMissing`).
- **entity_find.** bridge `game.entities`; `toggleState` is null until the bridge can read it.
- **select_weapon.** Goes through the game's own selection paths (`game.selectWeapon`): slot N ->
  `slotN`, command -> invnext etc., a specific weapon -> `CCSGOInput` MakeWeaponSelection ->
  usercmd `weaponselect`. Commands: invnext|invprev|invnextselect|invprevselect|lastinv. CS2 has no
  `use <weapon>` command.
- **sounds_drain / sounds_status.** bridge `sound.drain`: seq, hash, name, source entindex, guid,
  via slot, ageMs, origin/entityName when resolvable; bounded ring (256), `dropped` counts
  overflow; the hook installs on first use. sounds_status = `sound.status` / `sound.subscribe
  {mode}`: wrap (vtable wrap, hash+entity), listener (the game's slot-81 listener registry,
  name+guid), both.
- **game_wait.** Polls `game.local` server-side. `moved`: 3D distance, tick advanced. `stopped`:
  speed < 1 on two samples with the tick advancing. `at`: origin within `tolerance` (xy, default
  2), `zTolerance` (4), optionally view angles within `angleTolerance` (1, wrapped) and speed < 1
  (`requireStopped`). `roundLive` uses `game.rules` (no warmup, freeze, team intro, restart or
  round winner; local player alive); `roundRestarted`: `roundStartTime` grew or `roundsPlayed`
  changed, or the restart window was seen on the previous sample, and roundLive. `ammo`:
  `clip1` moved by `clip1Delta` (default -1; negative = at most, positive = at least) on the
  baseline weapon: a switch keeps waiting; `clip1` + `entindex` give a baseline read before the
  first sample. Returns `matched` with `before/after/delta` (rules echoed for round predicates).
  Fails fast with `PrerequisiteFailed` when not in game or dead (round predicates wait through a
  death). `timeoutMs` default 5000, `pollMs` default 100 (50..1000).
- **fire_weapon.** Per shot: release then tap attack (`frames` each), re-tapped every 200 ms until
  clip1 drops by one from the baseline read before the first tap (a semi-auto ignores a tap during
  its cycle); per-shot deadline `min(remaining timeoutMs, 1000 ms)`. Refused before sending
  (`PrerequisiteFailed`): not alive or in game, no active weapon, weapon not the one asked for, no
  clip (knife, clip1 -1), clip1 below `shots`. Then attack is released and `sound.drain` is polled
  up to 16 ticks until `shots` shot sounds (`Weapon_*.Single|Fire|Auto` from the shooter pawn)
  appear; BulletImpact sounds are reported too. `sounds` is null with a note when the bridge has no
  sound methods; a failed drain makes `ok` false. Returns `clip1Before/After/Delta, shotsFired,
  taps, released, sounds (with dropped), ticks, ok`. Attack is released after any error.
- **look_at.** Pitch/yaw from the eye (`game.local player.eye`; origin when `eye` false) to the
  target, sent as an `input.set` angle override and awaited until within 1 degree. `hold` false
  clears it after settling and reads the view back a few ticks later (`viewAfterClear`);
  `snappedBack` is true when that moved more than 1 degree from `achieved` (with a note), null when
  not read. Returns `angles, achieved, distance, inputCleared`. An entity target is a single-slot
  `game.entities` read by entindex (`EntityNotFound` when empty); a brush entity's origin may be the
  brush centre. An error after the override was set clears it.
- **use_entity.** Sequence: `sound.drain` baseline, aim (as look_at, held), press use for 4 frames,
  wait 32 ticks, clear the aim, drain sounds, re-read the entity. `evidence` lists use sounds (from
  the target's entindex always; or a name containing Button or Door within 128 units of the target,
  when the event has an origin) and an entity change (origin or angles). A target whose slot is
  empty afterwards is evidence too (`removed: true`). The use key is released and the aim cleared
  after any error. `toggleState` is not reported (the client schema has no field for it). Live
  example (cs_script_demo, 2026-10-12): func_button entindex 364..371 (`mdlchange.button.*`) at
  x 57, y 1394..1482, z 18; the eye is about 64 above the player origin, so setpos near the button
  first.
- **ent_fire.** Console.exec `ent_fire <target> <input> [value] [delay]` confirmed by the marker
  (as console_exec confirm). target `[A-Za-z0-9_!*.-]` 1..64, input `[A-Za-z0-9_]` 1..64, value up
  to 128 of `[A-Za-z0-9_.- ]` (quoted when it has a space), delay 0..60 s; a `;`, quote or newline
  is `InvalidArguments`. Kill/KillHierarchy on a wildcard or classname target (a name starting
  func_, prop_, trigger_, point_, env_, info_, logic_, game_, cs_, weapon_, player, ...) needs
  `allowDestructive`. Not a security boundary: point_servercommand and logic relays can run other
  inputs or commands. Returns the engine's warnings (Unknown command lines).
- **setpos.** console.exec `setpos_exact x y z` (and `setang_exact p y r` with `angles`;
  `setpos`/`setang` with `exact` false), then waits for `game.local` at origin (xy `tolerance`
  default 2, `zTolerance` 4, speed < 1 on two samples, view within `angleTolerance` 1 when angles
  given). `floor`: the same command, but the wait accepts any landing at origin xy on the ground
  (FL_ONGROUND) at or below `origin.z` (+zTolerance); reports `landedZ` and `dropped`; a fall past
  512 u or below `minZ` is `FellOutOfMap`. Coordinates |v| <= 32768, rounded to 0.001; angles
  validated like input_set (pitch +-89, roll +-180, yaw wrapped). `exact` false may land
  vertically offset: the zTolerance check decides, a larger offset is a `WaitTimeout`. Refused with
  `PrerequisiteFailed` before sending when an `input.set` angle override is active
  (`input.status`) or not in game/dead. Returns `after, toTarget, waitedMs`; a timeout is
  `WaitTimeout` with a hint when the origin was reached but the view was not. Once the command
  is sent every error is not retry-safe (`commandsSent`). `timeoutMs` default 4000.
- **game_snapshot / game_restore.** Snapshot reads `game.local`, `game.weapons`, `game.rules` under
  one lock; the store holds 32 (least recently stored dropped and named). `export` writes
  `artifacts/snapshots/<name>.json` before the store changes (path in `file`). Restore: `input.clear`
  (when advertised), pose via `setpos_exact`/`setang_exact` (verified by the `at` wait; a pose that
  does not settle is `pose.ok` false, not an error), health via `ent_fire !player sethealth`,
  armor via `give item_kevlar/item_assaultsuit`, missing weapons via `give`, then the active weapon
  via `game.selectWeapon` (after any give; a give into an occupied slot may drop the weapon there,
  not live-checked). One settle phase within `timeoutMs` (default 6000). Returns `restored,
  comparison ({field, wanted, got, ok}; a reason where the console cannot fix the mismatch),
  notRestored (velocity, ducked, clip1, reserve), approx (armor), extraWeapons (not dropped),
  commands, budgetExhausted`. Refuses before sending: another map or team (`PrerequisiteFailed`),
  an unknown name (`InvalidParams` lists the stored names). Once a command is sent, errors are not
  retry-safe (`commandsSent`).
- **round_start.** `ensure`: nothing is sent when the round is live; with cvars or shortcuts on a
  live round they are sent and nothing waits. A dead player, a player not on a team, or a set
  round winner falls back to the restart path (`fallback`, `fallbackReason`). In warmup it queues
  the cvars and `mp_warmup_end` and waits for roundRestarted; otherwise it queues the cvars and
  waits for roundLive. `restart`: cvars + `mp_restartgame 1`, waits for roundRestarted against the
  rules read before anything was sent. `input.clear` is sent first. Cvar names `[a-z0-9_]`, values
  numbers or simple tokens `[A-Za-z0-9_.-]`; refused: map changes, exec/alias/bind,
  mp_restartgame, mp_warmup_*, host_*, quit, disconnect. Commands go through console.exec with a
  confirmation marker (`warnings`). `timeoutMs` (default 8000) is one deadline for the whole call.
  Returns `mode, changed, fallback, fallbackReason, commands, warnings, before.rules,
  after.rules, after.player, waitedMs, ticks`. `freezetime`, `roundtime`, `respawn` map to
  `mp_freezetime`, `mp_roundtime`, `mp_respawn_on_death_ct/_t`.
- **screenshot / observe / ocr.** Frames come from the Vulkan capture layer (never an X grab).
  screenshot returns pid/exe/presentIndex of the frame; the inline image is full size up to
  1920x1080, `maxWidth` narrows it; `ocr: true` runs tesseract `--psm 6` (<= 10 s). observe: frame
  <= 5 s, OCR <= 10 s, game.tick only when the target is a wine-tools session whose bridge is the
  configured one (<= 3 s), `image` inline/small (<= 640 px)/none, `consoleLines` default 20.
  ocr: `psm` 6 or 11; `crop` is upscaled 2x before OCR.

## Server instructions (WP2.4)

`INSTRUCTIONS` in `src/lib.rs` is what an agent reads at `initialize`: purpose, first call
(`session_status`; the two flavours and which groups each has), intent gating (a question changes
nothing), the traps (console guard and `stop_session`, session limits, `pending:true` waits,
`BridgeBusy`, no console during a level load), "never step outside the tools" and a definition of
done. Guard: `tests/instructions.rs` (1800-character cap; every snake_case word must be a registered
tool or alias). `lib.rs::build_registry()` exposes the registry for that test.
