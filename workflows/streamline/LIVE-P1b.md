# P1 live re-check after the live fixes (2026-10-26, streamline/agent-ux at 5c0cfcb, bridge m3.16)

One Hammer tools session (`TOOL=hammer bridge/runtime.sh up`, addon `mcp_test`), driven through the
worktree's `target/debug/hammer-mcp`. I only tested and recorded; no code was changed. The raw
evidence is under `target/live-p1b/` in the worktree. `<NN>-*.txt` holds every JSON-RPC reply in
order, `scripts/` holds the drivers, and `bin/two.py` is a two-process driver (clients A and B,
timestamps on every line). `console-session.log` is the game console.log of the session.

## Versions

- hammer-mcp / client-mcp: `cargo build -j2` debug at 5c0cfcb (`build.log`).
- Bridge: `bridge/build-rs.sh` OK, export gates passed (`build-rs.log`). `bridgeVersion` **m3.16**,
  game build 2000933. Installed `cs2_bridge.dll` sha256 `4b3454d9…50d8`. The launcher and mmload are
  unchanged (`da0551c9…822f`, `e0f8b76d…737a`), see `install-status-after.txt`.
- The previously installed m3.15 (`ff1084ee…672c`, with launcher and mmload) is backed up to
  **`<repo>/target/bridge/backup-2026-10-26-p1b/`** (`SHA256SUMS`).
  m3.12 stays in `backup-2026-10-26-p1/`.
- m3.16 stays installed. Nothing in it broke Hammer.

## Checks

| # | Check | Verdict | Evidence |
|---|---|---|---|
| 1 | session_status shows m3.16 | PASS | `01-session_status.txt`, `01b-session_status.txt` |
| 2a | compile_map `novis` as a job, to the end | PASS | `02-compile.txt` |
| 2b | compile_map `stages:["world","nav"]` as a job, to the end | PASS | `02-compile.txt` |
| 2c | run_map during a compile of the same map gives CompileRunning + jobId + job_status | PASS | `02-compile.txt` |
| 2d | the map loads with run_map afterwards | PASS | `03-run_map.txt` |
| 3 | run_map over the good vpk passes the header check | PASS | `03-run_map.txt` |
| 4a | `particle_create {path:"particles/p1b/a.vpcf"}` (addon-relative) | PASS | `04-particles.txt` |
| 4b | particle_compile of a stale output: reason + `skippedByCompiler`; `force:true` compiles | PASS | `04-particles.txt` |
| 5a | set_addon addon_template, then open_map of its map gives `WrongEngineAddon`, `engineAddonSource:"container"` | PASS | `05-open_map.txt` |
| 5b | open_map of an mcp_test map reports engineAddon | PASS (finding A) | `05-open_map.txt`, `02a-open_map-before-hammer.txt` |
| 6a | 600-1023-byte line on each route: delivered or `Command too long` | PASS (finding B) | `06-console.txt`, `06b-console-limit.txt`, `06c-console-limit2.txt` |
| 6b | vtm `waitFor` does not match the command echo | PASS | `06-console.txt` |
| 6c | `cvar_get rcon_password` redacted, `redactedBy:"name"` | PASS | `06-console.txt` |
| 6d | nonexistent cvar gives `found:false`, not redacted | PASS | `06-console.txt` |
| 7 | BridgeBusy for a mutator while play_test holds the bridge | PASS (same process); second process sees `Busy` at hello | `07-bridgebusy.txt` |
| 8 | input_clear gives `held:[]` + `releasedHeld` | PASS | `08-input_clear.txt` |
| 9 | `runtime.sh down` | PASS | `runtime-down.txt` |

### 1. session_status
On a fresh session: `bridgeVersion m3.16`, 33 methods, hammer.dll `not_loaded`. After `runtime.sh
open maps/p1_test.vmap`: 59 methods, including `map.open`, `job.start`, `console.exec`, `cvar.get` and
`mesh.setFaceTexture`. `addon {name: mcp_test, source: document}`.

### 2. compile_map
- `novis` (bridge route): `-world -phys -bakelighting -nav` now pass the bridge allowlist. The job
  ended `state ok`, `OK: 18 compiled, 0 failed, 1 skipped, 0m:26s`, `stagesFrom:"preset"`,
  `vpkUpdated:true`. Bug 1 of LIVE-P1 is fixed.
- `stages:["world","nav"]`: `stageFlags ["-world","-nav"]`, `stagesFrom:"explicit"`, `OK: 12
  compiled, 0 failed, 1 skipped, 0m:04s`, `vpkUpdated:true`.
- `run_map {path:"maps/p1_test.vmap"}` 0.5 s into the novis job was refused at once with
  `type:"CompileRunning"` (`name:"EngineBusy"`), `data.jobId:"c1"`, `reason:"compile_running"`,
  `useInstead:{tool:"job_status", args:{jobId:"c1"}}`, and an `ignoreRunningCompile:true` escape in
  the message. Nothing was posted.
- After the jobs, `run_map` loaded p1_test: `state ok`, `gameLocal {inGame:true, map:"p1_test"}`,
  3.9 s.

### 3. run_map vpk header
The good vpk (222 572 bytes, written by the stages compile) loaded with no header complaint (`vpk.found`,
`state ok`). I made no negative case: the brief asks for the good file only. In P1 a corrupt vpk
killed the engine.

### 4. Particles
- `particle_create {path:"particles/p1b/a.vpcf"}`: `created:true`, `addon:"mcp_test",
  addonSource:"document"`, written under `content/csgo_addons/mcp_test/particles/p1b/`. Bug 2 of
  LIVE-P1 is fixed.
- First compile: `compiled:1, ok:true`. Then I touched the source so its mtime was newer than the
  `_c` (content unchanged). Without force the compiler said `OK: 0 compiled, 0 failed, 1 skipped`, and
  the tool returned `ok:false`, `skippedByCompiler:true` (top level and per file), and `reason:"the
  compiler skipped this input (it treats its output as current), but the compiled output is older
  than the source; retry with force:true (compiles with -f)"`. With `force:true`: `OK: 1 compiled`,
  `outputUpdated:true`, `ok:true`. Bug 3 of LIVE-P1 is fixed.

### 5. open_map
- `set_addon {addon:"addon_template"}`, then `open_map {path:"maps/xxx_mapname_xxx.vmap"}` →
  `-32602 WrongEngineAddon`, `data {engineAddon:"mcp_test", engineAddonSource:"container",
  selectedAddon:"addon_template", reason:"wrong_engine_addon"}`. The message gives the way out
  (`ADDON=addon_template bridge/runtime.sh up` + `--addon`). Nothing was sent, and the document stayed
  p1_test.
- `set_addon {addon:null}`, then `open_map {path:"maps/compile_test.vmap"}` → `opened:true`,
  `engineAddon:"mcp_test", engineAddonSource:"container"`, a new document `d2`, epoch 2.
- Then `open_map maps/p1_test.vmap` (still open in a background tab) → `existing:true, active:false`,
  with a note that no tool can switch Hammer tabs. This is recorded only. It means an agent that opens a
  second map cannot get back to the first one in the same session.

### 6. Console (p1_test running)
**The engine limit per command is 510 bytes, not 1023.** Measured on the bridge route with
`hostname "<value>"` lines read back through `cvar_get`:

| line bytes | engine | hostname read back |
|---|---|---|
| 200, 400, 500, 510 | runs | the full value |
| 511, 600, 1000 | `WARNING: Command too long... ignoring!` | unchanged |

- The binary search with unknown-command lines agrees: 510 runs, 511 is dropped (`06b`). The limit
  applies to each `;`-separated command, not to the whole line: a 702-byte chain of 14 short commands
  ran all 14 (`06b`).
- A separate, smaller limit applies to the command name: names over 128 characters get `Console
  command too long.` and do not run (129, 150 and 199 dropped; 128 ran as an unknown command, `06c`).
- **Bridge route**, single commands of 600/800/1000/1023 bytes: every one was dropped by the engine.
  The reply carried `deliveryWarning: "the engine logged \"Command too long... ignoring!\": a line was
  dropped, the command did not run"`, `endedBy:"marker"`. 1024 bytes is refused locally (`InvalidParams`,
  nothing sent).
- **vtm route.** I forced it by holding the bridge with a second hammer-mcp process. B's hello got
  `Busy -32007`, so B falls back with the note "the bridge could not be reached (Busy)…". The same
  600/800/1000/1023 lengths were each dropped with the same `deliveryWarning`. Each vtm call takes
  about 5.5 s (one vtmsend.exe launch per post).
- **Echo mask (vtm):** `echo p1b_wf_target_q` + `waitFor:"p1b_wf_target_q"` → `endedBy:"timeout"`,
  `matched:false`. The `_data = "echo p1b_wf_target_q"` echo line is in `lines` but was not matched.
  The same holds for `status` + `waitFor:"status"` (the `_data = "status"` echo was skipped). A
  pattern that matches real output still works: `p1b_wf_unk_q` + `waitFor:"Unknown command:
  p1b_wf_unk_q"` → `endedBy:"match"` on the engine's line, not the echo. Bug 6 of LIVE-P1 is fixed.
- **cvar_get:**
  - `rcon_password` → `redacted:true, redactedBy:"name"`, value null, with a `redactedNote`.
  - `sv_password` → `redacted:true` (flag `protected`, no `redactedBy`).
  - `p1b_nope_cvar` → `{found:false, name}` only, with no redaction fields.
  - `mp_c4timer` → "40".
  - Bug 8 of LIVE-P1 is fixed.

### 7. BridgeBusy
- **Same process, two concurrent calls:** `play_test {wait_ms 28000}` was started, and `save_map` 1 s
  later. save_map was refused after **10.00 s** with `type:"BridgeBusy"`, "the bridge stayed busy with
  another call for 10000 ms; nothing was sent", `retrySafe:true`. play_test finished normally (28.0 s,
  `passed:true`). After that, `save_map` ran (`skippedClean:true`). Bug 9 of LIVE-P1 is fixed.
- **Second process:** the bridge takes one client. B's `save_map` failed after 2.45 s with
  `-32007 Busy` at `control.hello`, which is a refusal and nothing was sent, but not the `BridgeBusy`
  path. B's `list_soundevents` (read-only) ran in 3.7 s with `addonNote: "the addon could not be
  confirmed against the active document (Busy); used \"mcp_test\" (from flag)"`.

### 8. input_clear
After `input_set {press:[duck, attack]}` (`held:[attack, duck]`), `input_clear` returned `held:[]`
and `releasedHeld:[attack, duck]`. Nit 3 of LIVE-P1 is fixed.

### 9. Teardown
`runtime.sh down` → "container cs2tools removed". `podman ps` is empty.

## Bugs and findings

A. **open_map cannot open the first map of a session.** On a fresh `runtime.sh up`, hammer.dll is not
   loaded, so `map.open` is not advertised. `open_map {path:"maps/p1_test.vmap"}` returns
   `NotImplemented` / `CapabilityMissing`, `missing:["map.open"]`, `blockedOn: "docs/data/bridge-protocol.md
   §7 method table"` (`02a-open_map-before-hammer.txt`). Nothing tells the caller what to do. The only
   way in was `bridge/runtime.sh open maps/p1_test.vmap`. After that, open_map works. A weak agent with
   only the MCP tools would be stuck here. Either open_map should fall back to the VToolMessage
   `open_asset` post when hammer.dll is not loaded, or the error should name that step.
   **Repro:** `TOOL=hammer bridge/runtime.sh up`, then `open_map {path:"maps/p1_test.vmap"}`.

B. **The local console limit (1023) is about twice the engine's (510).** Single commands of
   511-1023 bytes pass the local check, go out, and are dropped by the engine. The new
   `deliveryWarning` reports this correctly on both routes, so nothing fails silently. But the
   up-front refusal and its message ("the engine drops longer lines") name the wrong number. The
   engine limit is per `;`-separated command (510 bytes), not per line. Command names over 128
   characters are also dropped, with a different message (`Console command too long.`), which
   `deliveryWarning` does not detect. That case is minor: no real command name is that long.
   **Repro:** `console_exec {command:"hostname \"<500 x z>\""}` (511 bytes), then `cvar_get
   [hostname]` shows the value unchanged.

C. Minor: host-side refusals carry `"code": null` (`CompileRunning`, `BridgeBusy`), while bridge
   and parameter errors carry numbers (`-32602`, `-32011`, `-32007`). This is consistent with `code`
   being the wire code. A client that switches on `code` has to use `type` instead. Recorded only.

D. Minor: a second hammer-mcp process gets `Busy` at `control.hello` instead of `BridgeBusy`
   (check 7). The behaviour is safe. Recorded so that the docs do not promise `BridgeBusy` across
   processes.

## Open questions, answered

- **Engine limit for 600-1023-byte lines:** none of them run, on either route. The engine drops any
  single command over **510 bytes** (`WARNING: Command too long... ignoring!`). The limit is per
  `;`-separated command, so a longer chain of short commands runs. hammer-mcp now reports the drop
  as `deliveryWarning`.
- **Echo mask:** works. On the vtm route the `Handling VToolMessage … _data = "<command>"` block is
  returned in `lines` but never matched by `waitFor`. Real output after it still matches.

## Files left behind (test addon mcp_test only)

- `content/csgo_addons/mcp_test/particles/p1b/a.vpcf` and its `game/.../particles/p1b/a.vpcf_c`.
- `game/csgo_addons/mcp_test/maps/p1_test.vpk`, recompiled (world+nav last).
- `hostname`: the restore line `hostname "cs2-docker"` was sent before teardown but not read back.
  The cvar is not persisted, and the container is gone.

No commit was made.
