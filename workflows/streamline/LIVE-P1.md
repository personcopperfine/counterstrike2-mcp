# P1 live-test batch (2026-10-26, streamline/agent-ux at aadc6d9)

One Hammer tools session (`TOOL=hammer cargo bridge runtime up`, addon `mcp_test`), driven through the
worktree's `target/debug/hammer-mcp`. Tested and recorded only; no code was changed. Raw evidence is
under `target/live-p1/` in the worktree (`<NN>-*.txt` = every JSON-RPC reply in order; `scripts/` =
the drivers that produced them; `console-session1.log` = the game console.log of session 1).

## Versions

- hammer-mcp / client-mcp: `cargo build -j2` debug at aadc6d9 (`target/live-p1/build.log`).
- Bridge: `cargo bridge build` OK (exports gate passed, `target/live-p1/build-rs.log`), `hello.build`
  **m3.15**, game build 2000933. Installed: `cs2_bridge.dll` sha256 `ff1084ee…672c`,
  `cs2mcp_launch.exe` `da0551c9…822f`, `cs2mcp_mmload.dll` `e0f8b76d…737a`
  (`target/live-p1/install-status-after.txt`).
- Previously installed (m3.12, `1c4a8a62…c7fab`) backed up to
  **`<repo>/target/bridge/backup-2026-10-26-p1/`** (with
  `SHA256SUMS`). `install-rs.sh install` also made its own `backup-rs-<UTC>`.
- The new bridge stays installed. Hammer was not broken by it. The one session crash came from a
  test input I made on purpose (check 7, a corrupt vpk), not from the bridge.

## Checks

| # | Check | Verdict | Evidence |
|---|---|---|---|
| 1 | session_status: m3.15, `console.exec`/`cvar.get`/`mesh.setFaceTexture`, addon | PASS | `01-session_status.txt` |
| 2 | WP0.2 dirty flag after 256+ edits | PASS | `02-dirty.txt`, `03b-compile-full.txt` |
| 3a | WP1.4 compile_map `full` and `nav` as jobs | PASS | `03b-compile-full.txt` |
| 3b | WP1.4 compile_map `novis` / `stages:[...]` | **FAIL** (bug 1) | `03-compile-novis.txt`, `03b-compile-full.txt` |
| 3c | particle_compile folder, several .vpcf in one call | PARTIAL (bugs 2, 3; multi `-i` SKIP) | `03c/03d-particle-folder.txt`, `04c-addon-check-file.txt` |
| 4a | WP1.1 open a map in another addon | **FAIL** (bug 4) | `04-addon.txt` |
| 4b | writers report the right addon | PASS (mcp_test only; see 4a) | `04c-addon-check-file.txt`, `03b-compile-full.txt` |
| 4c | `AddonCheckFailed` / `addonNote` | PASS for file tools, with a gap (finding 9) | `04b-addon-check.txt`, `04c-addon-check-file.txt` |
| 5 | WP1.2 apply_layout | PASS | `05a-layout.txt`, `05b-undo.txt`, `05c-layout32.txt` |
| 6 | WP1.5 align_texture | PASS; rotate-vs-UI PARTIAL | `06-align.txt`, `06b-align-undo.txt`, `06c-align-fit.txt` |
| 7 | WP1.3 console_exec / cvar_get | PASS with findings (5, 6, 7) | `07a`…`07e-*.txt`, `console-session1.log` |
| 8 | WP1.6 play-test | PASS | `08a`…`08e-*.txt` |
| 9 | `console_exec "quit()"` refused, then down | PASS | `09-quit-guard.txt` |

### 1. session_status
`bridgeVersion m3.15`, 59 methods once hammer.dll loaded, including `console.exec`, `cvar.get` and
`mesh.setFaceTexture`. `addon {name: mcp_test, source: document, documentAddon: mcp_test, flag: mcp_test}`.
Before a document was open: 33 methods, hammer.dll `not_loaded`, `source: flag`.

### 2. WP0.2
On `maps/p1_test.vmap` (a copy of compile_test made for this run): apply_layout (32 meshes + 8
entities) plus 250 `set_node_transform`. `topSerial` reached 293 (Hammer caps `undoDepth` at 100).
Before save: `dirty:true`. After `save_map` (`skippedClean:false`): `dirty:false`. `compile_map
{mode: full}` started and finished `ok`. No `UnsavedChanges`.

### 3. WP1.4
- `full` (bridge route): `state ok`, `OK: 20 compiled, 0 failed, 0 skipped, 0m:39s`, `compiled:20`
  for one input (dependencies). `stageFlags: []`, `stagesFrom: "compiler default"`, `stages` lists the
  five. The vpk mtime advanced.
- `nav`: `OK: 1 compiled, 0 failed, 1 skipped`, `stageFlags:["-nav"]`. OK.
- `novis` and `stages:["world","nav"]`: refused, see bug 1.
- Particles: `particle_compile {path:"csgo_addons/mcp_test/particles/mcp_probe"}` (4 .vpcf) and
  `particles/p1` (2 new files from `particle_create`) both ran. On the bridge route each file is
  **its own `job.start`** (`j3`…`j6`, `j8`/`j9`). The multi-`-i` podman command (the open WP1.4 FIXME
  item) is never used while `job.start` is advertised, so it is still not live-tested (**SKIP**).
  Child resolution works: `probe.vpcf` was reported `referencedBy: live_kids.vpcf`, `upToDate:true`.

### 4. WP1.1
- `set_addon {}` lists the 5 addons. `set_addon {addon:"mcp_tset"}` gives `AddonNotFound`,
  `didYouMean:["mcp_test"]`, and the selection is unchanged. `set_addon {addon:"addon_template"}` gives
  `source:set_addon`. `set_addon {addon:null}` follows the document again.
- Opening another addon's map: fails, bug 4. ti_convoy was not touched. The only other addon with a
  small map is Valve's `addon_template`. Nothing in another addon was written.
- Writers: `particle_create` → `addon:"mcp_test", addonSource:"document"`. `compile_map` →
  `addon` + `addonSource:"document"`.
- `addonNote` (read-only, no bridge methods): `list_soundevents` started while `play_test {wait_ms
  14000}` held the bridge. It ran after the 10 s lock wait and carried `addonNote: "...the bridge
  stayed busy with another call for 10000 ms); used \"mcp_test\" (from flag)"`.
- `AddonCheckFailed` (file writer): `particle_create` in the same situation was refused after
  10.0 s: `-32010 AddonCheckFailed`, `retrySafe:true`, `data:{addon, addonSource}`. Nothing was
  written. The same call afterwards succeeded.

### 5. WP1.2 apply_layout
- 3 blocks (box, wedge, cylinder), 1 tetrahedron `mesh` and 1 `info_target`. `created` returned 5
  items with nodeIDs 108–112, `undoGroups:5`, `bridgeRequests:5`, no warnings.
- 4 consecutive `mesh.create` calls, all under the layout's single `expectDoc`, succeeded (32 in
  the larger layout). This closes FIXME WP1.2 item 3: the live fingerprint does not move on edits.
- **Per-mesh latency:** 32 meshes + 1 entity batch (8 entities) = 0.9 s for the whole call, about
  27 ms per `mesh.create` round trip. 4 meshes + 1 batch took 0.1 s. `MAX_LAYOUT_MESHES = 32` uses
  under 1 s of the 45 s budget.
- `undo_last` ×5 popped "MCP: batch", then 4 × "MCP: create mesh". `undoDepth` went 5 → 0. Then
  `get_mesh 108` / `get_entity_live 112` returned `MeshNotFound` / `EntityNotFound`, and a 6th
  undo answered "no undo group, nothing sent".
- `dryRun:true`: `sent:false`, `undoDepth`/`topSerial` unchanged (0/0) and no new lines in the
  bridge log. Nit: its `next` is `apply_layout {dryRun:false}` with no `layout`/`path`, so the call
  as suggested would be invalid (finding 10).

### 6. WP1.5 align_texture
Box `min [40,-760,0] max [168,-632,128]` (128 u, off the 128 grid), nodeID 153, top face = index 1.
- `world` on the top face: `axisU [1,0,0,0]`, `axisV [0,-1,0,-0]`, `scale [0.125,0.125]`. A fresh
  `create_block` is already world-aligned, so the values did not change. `world` on all faces: walls
  get `[1,0,0]/[0,0,-1]` or `[0,1,0]/[0,0,-1]`, offsets 0.
- Undo: from a modified state (rotated 30°, justified) → `world` → `undo_last` → a probe write
  read back exactly the pre-`world` axes, offsets and scale (`06b-align-undo.txt`). It takes one undo
  entry per call ("MCP: set face texture"). The first byte compare of saved .vmaps was inconclusive
  (DMX element ids change on every save) and is not used as evidence.
- **Rotate sign:** from world alignment on the top face (normal +z), `rotate:90` gives axisU
  `[0,-1,0]`, axisV `[-1,0,0]`. `rotate:-90` gives `[0,1,0]`/`[1,0,0]`, and `rotate:30` gives
  `[0.866,-0.5,0]`. So a **positive value turns the axes clockwise seen from the face's front**
  (+x → −y on a floor). I could not compare this with Hammer's own rotate field or
  RotateTexture90CW: there is no headless way to read the face-edit UI. That part is unverified.
- **Justify fit, 128 u grid case (FIXME 29 shape):** `world` then `justify fit` on the off-grid
  top face gives `scale [0.25,0.25]`, `axisU.w -160`, `axisV.w -2528`. That is exactly one 512 px
  tile across x 40..168 / y −760..−632, with u = 0 at x = 40. A side face gave the same (`-160`,
  `512`). `justify left` after fit changed nothing. Justify on an already-rotated face gives
  asymmetric scales (`[0.2165, 0.3415]`); that is Hammer's computation, recorded only.
- `mode set` round-trip: `shift [16,32]`, `scale [0.5,0.125]` read back exactly.
- Refusals: `texSet:1` → `-32603 TextureSetUnknown` ("face 0: the texture parameters of set 1
  could not be read"), `retrySafe:true`, `outcomeUnknown:false`, `undoDepth` not bumped. `faces:[99]`
  → bridge `-32602` "index 99 out of range (the mesh has 6 faces)". justify+rotate and `set` with no
  field were refused locally. nodeID 999999 → `MeshNotFound`.

### 7. WP1.3 console
- **Route `bridge`:** every line ≤ 1023 bytes. `endedBy:"marker"`, `markerSeen:true`, about 50–100 ms.
  - Output reaching console.log: `status` and `exec` errors do. `echo`, a bare cvar name
    (`mp_c4timer`), `cvarlist`, `find` and `help` print **nothing** console.log sees. The tool then
    says so in `notes`.
  - The marker's `Unknown command: cs2mcp_mark_…` line is in console.log on both routes.
  - `waitFor` on the bridge route can only match output that does reach console.log: `echo
    p1_wait_target` + `waitFor` ran to `timeout` (1501 ms) with `markerSeen:true`.
- **Route `vtm`** (forced with lines > 1023 bytes):
  - The marker line IS echoed. `endedBy:"marker"`, and the output is correctly cut before the
    `Unknown command` line.
  - However, the engine rejects such lines: `WARNING: Command too long... ignoring!` (1117 and
    1135-byte lines). See finding 5.
  - `waitFor` matched the engine's `Handling VToolMessage … _data = "<the command>"` echo, so on the
    vtm route a pattern that occurs in the command text matches immediately (finding 6).
- **`;` split:** yes on both routes. A raw vtm post `p1_unk_vtm_a; p1_unk_vtm_b` and console_exec
  (bridge) `p1_unk_br_a; p1_unk_br_b` each produced two `Unknown command` lines
  (`07b-console-excerpt.txt`).
- **Idle level** (after `run_map {unload:true}`): `game_type 0; game_mode 1; mp_c4timer 39` ran on
  the bridge route, with the note "the engine is at its idle level…apply to the next run_map".
  `cvar_get` then read 0 / 1 / 39.
- **cvar_get** (in game):
  - Values: `mp_c4timer` int32 "41" after `console_exec mp_c4timer 41` (default "40").
    `sv_cheats` bool "true". `game_type` "0", `game_mode` "1". `mp_roundtime` float32 "1.92".
  - `nope_p1_cvar` → `found:false`. `sv_password` → `redacted:true`, value null, flag `protected`.
  - Writes through `console_exec` were read back by `cvar_get`: `mp_freezetime` 15 → 0,
    `mp_ignore_round_win_conditions` false → true.
  - A direct console-print comparison is impossible: none of the cvar print commands reach
    console.log (see above).
  - `rcon_password` is NOT `protected` in this build, so its value would be returned unredacted
    (finding 8).
- **Map-changing refusals:** `map`, `changelevel`, `retry`, `restart` and `connect` are refused, `ConsoleCommandRefused` +
  `useInstead: run_map`. `disconnect`, `quit` (also after `;`), `rcon_password`, and `alias`/`bind`
  bodies are refused. `exec autoexec` is sent, as designed.
- **Trailing `Loading map` line:**
  - A failed load of a bad name (`map p1_missing_zz`, posted raw over vtm) prints only `map
    p1_missing_zz:  invalid map name`, with no `Loading map` line. Afterwards `console_exec status`
    ran normally (not `EngineBusy`).
  - An aborted load from a corrupt vpk (4 KiB random `maps/p1_junk.vpk` that I placed in mcp_test,
    removed since) is not a trailing-line case: the engine logs `FATAL ERROR: VPK directory … corrupt`
    and the whole tools process exits. The session had to be restarted (`runtime-up2.txt`).
    `run_map` would load such a file the same way: it checks only that the vpk exists.

### 8. WP1.6 play-test
- `game_state`: one read, plus `samples:20` → `originStepStats` per axis (all zero steps while
  standing).
- The first walk attempt did not move the player: the round had just reset into
  `mp_freezetime 15` (competitive). After `console_exec mp_freezetime 0; …` walking worked.
- `input_sequence` forward 64 ticks + 8 release ticks: y −32 → 181.6 → 206.4, final 208.2 (+240 u),
  `inputCleared:true` (`08b-play.txt`).
- **release+tap (m3.15):**
  - With `jump` held (one jump, then standing), `input_set {release:[jump], tap:[jump]}` in one
    call produced a **second jump** (z 0 → 55.8 → 0).
  - Control: `tap` alone while held gives no jump.
  - With `duck`: held → `D…`, release+tap (32 frames) → one `.` gap frame, then `D×11`, then
    released. Re-press confirmed.
- `play_test` passing run (7 steps: input back, `wait_state player.origin[1] lt y0-40`, input
  stop, `wait_log` (since start) for a vtm-posted `Unknown command: p1_pt_ping_ok`, `expect_state`
  eq/near/contains): `passed:true`, `endedBy:"complete"`, `inputCleared:true`.
- `play_test` failing run: `expect_state player.health eq 12345` → `passed:false`,
  `endedBy:"failed"`, `failedStep:2`, error `{got:100, wanted:12345}`, `inputCleared:true`. The
  player then decelerated to a stop.
- `wait_log` timeout → a clean step failure. An unknown `kind` → `InvalidParams` before anything is
  sent.
- `game_local` → the same payload plus `deprecated:{alias:"game_local", use:"game_state"}`.

### 9. quit guard
On a fresh session: `quit()`, `quit`, `QUIT`, `"  quit  "`, `echo x;quit()`, `exit`,
`Test_ExitProcess`, `crash`, `echo "a";quit`, `quit;`, `+quit` and `alias z "quit()"` were all
`ConsoleCommandRefused`, with nothing sent. The bridge was still connected afterwards. Then
`runtime.sh down`; no game container is running.

## Bugs and findings

1. **compile_map `novis` and every `stages` list are refused on the bridge route (P0: a mode that
   worked on the podman route).** hammer-mcp sends `-world`/`-phys`/`-vis`/`-bakelighting` as job
   flags, but the bridge's `job.start` allowlist (`crates/cs2-bridge/src/ops/job.rs:336`
   `PLAIN_FLAGS`) only has `-nav -fshallow -quiet -unbufferedio -noassert -nop4 -f`. Repro: open any
   map, `compile_map {mode:"novis"}` or `{stages:["world","nav"]}` →
   `InvalidParams: the bridge refused the job request: flag "-world" is not allowed`. Nothing runs.
   Since `job.start` is advertised whenever the bridge is loaded, novis is unusable in practice.
   `full` and `nav` work.
2. **particle paths are content-root relative, and the error does not say so.**
   `particle_create {path:"particles/p1/p1_a.vpcf"}` and `particle_compile
   {path:"particles/mcp_probe"}` fail with `WriteOutsideAddon: …/content/particles/… is outside
   content/csgo_addons/mcp_test`. Only `csgo_addons/mcp_test/particles/...` works. The schema says
   "a .vpcf, or a folder, of the active addon", while `compile_map`/`open_map` take addon-relative
   `maps/...`. This predates WP1.1 (`vmdl/paths.rs` `candidate()`, from e5fc988). A weak agent will
   hit this first.
3. **particle_compile reports a compiler skip as a failure, with no reason.** `live_a.vpcf`: the
   compiler said `OK: 0 compiled, 0 failed, 1 skipped` (job log `cs2mcp_jobs/j3-mvpmdfj3.log`). The
   `_c` (04:45:11) is older than the source (04:45:33), so the mtime rule rejects `upToDate`. The
   file shows `ok:false, failLines:[]`, and the whole run is `ok:false`, with no text saying why.
   Either accept the compiler's skip or say "compiler skipped it but the output is older than the
   source; touch the source or compile with -f".
4. **open_map cannot open a map in another addon (WP1.1 gap).** `set_addon
   {addon:"addon_template"}` then `open_map {path:"maps/xxx_mapname_xxx.vmap"}`: the host precheck
   passes (the file exists in addon_template), but the bridge gets the bare content path and resolves
   it under the engine's own `-addon` (mcp_test), returning `AssetNotFound {reason:"no_such_file"}`.
   So there is no tool path to "open a map in another addon". The precheck and the bridge also
   disagree about which addon is meant.
5. **The console_exec vtm fallback for long lines cannot work.** Lines that don't fit the
   bridge's 1023-byte buffer go to vtm, but the engine itself drops them (`WARNING: Command too
   long... ignoring!`). The reply still says `endedBy:"marker"` with no warning, so the caller has
   to spot the WARNING among the lines. Either refuse lines over the engine limit up front, or turn
   that WARNING into a `deliveryWarning`. Only the length-forced vtm case was testable: the bridge
   never answered `-32011` in a `-tools` session.
6. **On the vtm route, `waitFor` matches the engine's echo of the command itself.**
   `ToolFramework2` logs `Handling VToolMessage … _data = "<command>"`. A `waitFor` regex that
   occurs in the command text matches immediately (`endedBy:"match"`, `matchedLine` = the `_data`
   line). Lines of that echo block should be skipped for matching.
7. **On the bridge route, `echo` / cvar prints never reach console.log** (`07a`, `07c`), so `waitFor`
   on `echo` output always times out. This is documented in spirit ("some commands print nowhere"),
   but AGENT.md's example use of `waitFor` should say that only `status`-class output reaches
   console.log in a `-tools` session.
8. **`cvar_get rcon_password` is not redacted.** The cvar has no `protected` flag in 2000933
   (`flagsRaw 67764224`). The value was empty here; a set one would be returned. The guard refuses
   setting it but not reading it.
9. **AddonCheckFailed never triggers for bridge-backed mutators.** `save_map`, started while
   `play_test` held the bridge, waited 13.6 s and then ran (`skippedClean`), instead of being refused
   at 10 s. `Registry::call` runs `require_caps` (bridge lock, no timeout) before `before_call`. This
   is safe (it ran on a re-checked addon), but the documented refusal applies only to tools without
   bridge methods. Either document that or bound the wait.
10. Minor:
    - The apply_layout dryRun `next` (`{dryRun:false}`) omits the layout/path.
    - `run_map` loaded `p1_test` while a `compile_map nav` job for the same map was running (only a
      `staleCompile` note).
    - The `input_clear` reply's `held` can still list the buttons it just cleared (`08d-clear.txt`;
      the release does happen).
    - Hammer-mcp's quit refusal says "use stop_session instead", but hammer-mcp has no
      `stop_session`.
    - `particle_create` returns string `nextStep` where other tools return a `next` array.

## Open live questions, answered

- **Marker echo on vtm:** yes, `Unknown command: cs2mcp_mark_…` appears and ends the output
  correctly.
- **`;` split:** yes, on both the vtm and bridge routes.
- **Trailing `Loading map` line:** not for a bad map name (`invalid map name`, no Loading line).
  A corrupt vpk kills the engine instead (FATAL ERROR), so there is no lingering line either.
- **Per-mesh latency:** about 27 ms per `mesh.create` (32 meshes + entity batch in 0.9 s).
- **Rotate sign:** +deg turns the texture axes clockwise seen from the face front (floor: U +x →
  −y for +90). It has not been compared with Hammer's UI field.
- **One `expectDoc` per layout:** works live (32 consecutive `mesh.create`).
- **cvar.get engine calls:** work live (FindConVar / GetConVarData / member reads match values set
  through the console).

## Files left behind (test addon mcp_test only)

- `content/csgo_addons/mcp_test/maps/p1_test.vmap`
- its `game/.../maps/p1_test.vpk`
- `content/csgo_addons/mcp_test/particles/p1/p1_a.vpcf`, `p1_b.vpcf`, plus their `_c`
- recompiled `_c` of `particles/mcp_probe/live_b`, `live_kids`

The junk vpk was removed. No commit was made.
