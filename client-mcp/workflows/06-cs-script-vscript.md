# 06 — CS script (point_script) and VScript testing

## Goal
Run a map's scripts under test: load a test map, trigger script entry points, capture script output
and assertions, and report pass/fail — for CS2's JavaScript/TypeScript `point_script` ("CS script")
first, VScript second.

## Status (2026-10-11): survey done, both kinds blocked

`script_run` validates its arguments and returns `NotImplementedYet` with `blockedOn`, the missing
piece and the evidence (`client-mcp/src/tools/script.rs`). It stages, launches and sends nothing.
The judge, staging-name and command builders live in `client-mcp/src/script/` and are tested, but no
runner calls them yet.

- **VScript: blocked.** Retail CS2 creates a `CNopVM` for the game's VM, so no script runs.
  `script_reload_code` is a no-op. The bare `script` command does not exist in CS2
  (`GameTracking-CS2/DumpSource2/commands.txt`: `script_reload`, `script_reload_code` at 2937-2942
  only). Evidence: `idabins/docs/interfaces/IScriptManager.md:19-22, 127-145`,
  `idabins/worknotes/ifacesurvey/B21-scratch.md:264`. Unblock: a build that selects `CLuaVM`, or a
  bridge `IScriptManager::CreateVM(1)` driving `CompileScript`/`Run` (`IScriptVM.md` slots 13-15).
- **cs_script: blocked.** The script object exists only inside a `point_script` entity, and that
  entity needs a `cs_script` keyvalue naming a compiled `vjs` resource
  (`idabins/docs/interfaces/CCSScriptSystem.md:165-183, 290-291`). The image has no compiler run and
  no staged addon. The shipped `maps/scripts/rush_001.vjs_c` is in `pak01_dir.vpk`; `de_dust2.vpk`
  has no `.vjs_c` in its tree, and whether `de_dust2.vmap_c` places a `point_script` is not verified.
- Live probe that needs no test map (orchestrator): on a `server_up` de_dust2 session, grep
  `server.log` for `VSCRIPT: Started VScript virtual machine using script language 'Nop'`
  (the format string is in `IScriptVM.md` §Who calls it, server VM setup). A `Nop` line confirms
  VScript is a stub on that server. Expect no `cs_script` lines on de_dust2 unless it has a point_script.

## Prerequisites
- 03, 04. Facts from the trees: `v8system.dll`/`libv8system.so` + `v8*.dll` ship (V8 engine;
  `Source2V8System001`), shipped maps carry `maps/scripts/*.js` and `maps/<map>.js` in `pak01_dir`
  (`../GameTracking-CS2/game/csgo/pak01_dir/maps/scripts/rush_001.js`), `vscript.dll`/`libvscript.so`
  exist with `CScriptManager::CreateVM`, `CLuaVM`, `CNopVM` entries in `../idabins/signatures/vscript.json`;
  `pulse_system.dll` (Pulse graphs) also ships. None of this has a flow page yet.
- Reversing budget: a `research/script-survey.md` pass with ida-mcp on `v8system.dll.i64` and
  `vscript.dll.i64` (exports, console commands, log channels) before designing tools.

## Steps
1. **Survey** (research): how `point_script` loads (`.vts`/`.js` under `scripts/`?), the console
   commands/convars (`script_*`, `cs_script_*`?), log channel names for script output, whether
   script errors reach `console.log`; same for VScript (`CLuaVM` suggests Lua — is the VM enabled
   in the csgo mod or `CNopVM` only?). Write it up with FORMAT tags.
2. **Test-map template** (`research/testmaps/`): a tiny `.vmap` with `point_script` wired to a
   `logic_auto`, plus a script that prints `SCRIPT_TEST_BEGIN/PASS/FAIL <name>` markers (so the
   judge from 04 applies unchanged); compile via hammer-mcp's `compile_map` plan or
   `resourcecompiler` directly; addon `mcp_test`.
3. **Tools**: `script_run {map, script?, args?, timeoutMs}` = stage script (content tree) ->
   compile if needed -> `run_testscript` suite that `map`s it, waits `FinishedMapLoad`, fires
   `ent_fire <target> <input>` triggers, waits for markers -> `{passed, output[], errors[]}`;
   `script_log_tail` filtered to the script log channel.
4. **Hot reload**: find the reload path (`script_reload`? map restart) to iterate without a full
   boot; fall back to `round` reset (`mp_restartgame 1`) per run.
5. **VScript**: repeat 2–3 once the survey says what is executable in retail/`-tools` builds.

## Definition of done
- A `point_script` test map runs through `script_run` and the markers judge it; script errors are
  surfaced with file/line from the log.

## Open questions
- Everything about the scripting surfaces is unsurveyed here: file extensions, API, console
  commands, whether `-tools`/`sv_cheats` is required, TypeScript compile step (`tsc` in our image?).
- VScript in CS2: `vscript.dll` exists, but is any VM other than `CNopVM` reachable from a map?

## Evidence
- Module presence and sig DB entry names [D]; shipped `.js` map scripts in GameTracking [D].
