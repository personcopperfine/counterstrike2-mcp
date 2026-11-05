# 04 — Unit tests in the real game (`CTestScriptMgr`)

**Status:** steps 1-3 implemented in `client-mcp` (`src/testscript/`, `src/tools/testscript.rs`), not yet
live-verified: no container has run a suite through the server. Step 4 (parked harness, many tests, JUnit)
is **TODO**, see the end of this file.

## Goal
`run_testscript` runs a suite deterministically (boot -> steps -> checkpoints -> pass/fail exit),
collects evidence, and judges it; the parked-harness mode runs many small tests per session with
per-test reset, producing JUnit — porting `run_vtest.sh` + `gametest/runner.py`.

## Prerequisites
- 01, 03. Read `../idabins/worknotes/vtest/README.md`, `vtest-scratch.md` (script semantics:
  `Test_Wait`, `Test_WaitForCheckPoint <name> [once]`, `Test_RunFrame`, `Test_SetEvent` = named Win32
  event, `Test_Run <fragment> args` splices `.vtest`, loops, `Test_ExitProcess`; failures are
  `Plat_FatalError`), `docs/interfaces/ITestScriptMgr.md`, `worknotes/vtest/suites/*`,
  `gametest/{runner.py,game.py,tests/}`.

## Steps
1. **Suite model** (`src/testscript`): suite = body + fragments; render with the rules
   (`;` splits always; only `Test_*` block; markers through `mark.vtest` barrier; end with `quit`
   then `Test_Wait 30` + `Test_ExitProcess 3` hang guard). Ship the POC fragments as embedded
   assets (`mark`, `setup_player`, `pos`, `snap`, `harness`, `sandbox`).
2. **`run_testscript {suite | inline, demo?, timeoutMs, judge?}`**: stage to `<app>/vtest/`
   (WineTools) or the Linux tree; launch with `-insecure -testscript /app/vtest/<suite>.txt`
   (no `-testmode`: it quits `-tools` sessions ~18 s after boot); stream
   `TESTSCRIPT: CheckPoint -> '<name>'` lines; wait for exit; collect `console.log`, `screenshots/vtest_*.png`,
   `vtest_*.dem|webm|wav` into `artifacts/<suite>-<ts>/`; run the judge.
3. **Judges**: port `check_move_jump.py` logic as a reusable assertion library (markers in order,
   `getpos` deltas, PNG exists + non-blank, demo size) in Rust; keep Python judges runnable from
   `research/` for ad-hoc suites. Return `{passed, checks[], artifacts[]}`.
4. **Parked harness + many tests**: `harness.txt` boots, spawns, parks at `VTEST_PARKED`; load the
   bridge; per test: reset (`soft` = `input.clear` + `setpos/setang` baseline; `round` =
   `mp_restartgame 1`; `hard` = `map` reload), `checkpoint gt_begin_<t>` -> actions (`input_set`,
   `console_exec`) -> asserts (console samples, `game_tick`, captures) -> `gt_pass_|gt_fail_<t>`;
   release with `test.checkpoint vtest_release`. Emit JUnit XML.
5. **Engine checkpoints** usable as waits: `map_load`, `client_connected`, `FinishedMapLoad`,
   `Wrote_Screenshot`, `frame_end`, `client_disconnected`, `DemoPlaybackStarted/Finished`;
   ours via bridge `test.checkpoint` (dropped while the script is in `Test_Wait` — use
   `Test_WaitForCheckPoint` barriers).
6. **MCP shape**: `run_testscript` is long-running — return a `runId` immediately plus
   `test_status {runId}` / `test_wait {runId, timeoutMs}`; stream progress to stderr logs.

## Definition of done
- `run_testscript move_jump` passes 17/17 on WineTools via the server; `gametest` suite runs >= 3
  tests in one session with `soft` resets and a JUnit file; both produce artifact dirs.

## Open questions
- LinuxClient `-testscript` support (01 open question) — if yes, the same suites run headlessly
  without the bridge (markers + screenshots only).
- `Test_SetEvent` named events as host-visible sync (same wineserver only) — probably not needed
  given checkpoints + VToolMessage.
- `game.saveRestore`/`load` is not a usable reset; is there a faster hard reset than `map` reload (~11 s)?

## Evidence
- `run_vtest.sh move_jump` PASS x2 (17/17), `run_gametest.sh` with reset modes and JUnit [proven,
  2026-10-10]; script semantics `[V]` in `vtest-scratch.md`.

## Implementation notes (2026-10-11)
- Suite model: `src/testscript/mod.rs`. `render` normalises CRLF, strips `//` comments (to end of line,
  before the split, ITestScriptMgr.md:94), splits on `;`, keeps whole-line comments verbatim, and matches
  specials case-insensitively. It expands `mark <name>` to `Test_Run mark <name>`; rejects a bare
  `Test_Checkpoint`, unknown fragments, and `quit` / `exit` / `Test_ExitProcess` anywhere but the trailer;
  and ends with `quit` / `Test_Wait 30` / `Test_ExitProcess 3`. The POC move_jump.txt renders to itself
  byte for byte. Fragments and suites are embedded from `assets/vtest/` (README there).
- Staging writes `<app>/vtest/<name>.txt` and every fragment verbatim, but a file is rewritten only when its
  content differs (temp file + rename). No `-testmode`. Order in `run_testscript`: render check, budget
  check (refuse below 5 s), `preflight` (cap and one-session-per-kind), then staging, then launch. A refused
  run writes nothing; a failed launch removes its inline script.
- Judges: `src/testscript/judge.rs`. The `move_jump` preset is check_move_jump.py ported in order (17 checks;
  line refs in the code). Each PNG is read once and decoded once. Added checks carry "(added)" in their
  names: markers in order, non-blank screenshots (frame stats), and the optional `demo` size check.
- Runs: `src/testscript/run.rs`. `spawn_drive` starts `drive` under a supervisor (a panic or cancellation
  finishes the run as `error`). `drive` polls the container and reads console.log with
  `ConsoleLog::read_new_lines`, draining it to the end on every poll, so the tail after an exit is recorded.
  It records checkpoint, quit and fatal lines (timeline capped at 2000, `timelineTruncated`).
  * exit: judged in `spawn_blocking`; `passed` when the exit check and every judge check pass;
  * timeout, or quit logged but no exit after 45 s (`QUIT_GRACE`, longer than the suite's own
    `Test_Wait 30` hang guard, so exit 3 is what gets observed): the container is removed with `rm -f`,
    and the error says whether that succeeded (`container X stopped` / `stopping container X failed: ...`);
  * a vanished container is `error`.
- Evidence: copies are taken only when mtime is at or after the call. After collection, older `vtest_*`
  PNGs in `csgo/screenshots/` and `vtest_*` media in `csgo/` are pruned from the game dir (the POC's
  cleanup intent), so the next run starts clean. The copies in earlier artifacts dirs are kept.
- Table: `RunTable` keeps every running run and the newest 64 finished ones.
- Lifetime: runs live in the server process. On exit (stdin closed), `client_mcp::shutdown` removes the
  containers of runs still in progress (best effort, logged on failure). Finished runs' artifacts stay on
  disk; launched sessions (`launch_session`) are not stopped and remain until `stop_session`.
- Busy: a refused run gets `KindBusy` with `data.container`, plus `data.runId` when one of this server's runs
  owns the container. Other busy refusals keep the runtime's shape.
- Exit codes: 0 with passing checks = `passed`; a failing check = `failed`; exit 3 is the hang guard and fails
  the run; timeout, a stalled quit, or a vanished container = `error`. A container whose exit code cannot be
  parsed reports -1, never 0.
- `test_wait`: returns the status; a run still going has `pending: true` and `next: "test_wait {runId}"`.
- Fixtures: `fixtures/testscript/move_jump_pass.console.log` is 17 lines selected verbatim (not contiguous)
  from `idabins/worknotes/vtest/artifacts/move_jump-20261010-031947/console.log`. The POC file has CRLF
  endings and the fixture keeps them. `move_jump_nojump.console.log` is derived: the midjump z is set to the
  moved z.
- `round_start` (`src/tools/game_round.rs`): the per-test round reset. `ensure` sends nothing when game.rules
  says the round is live; a dead player, a spectator or a set round winner falls back to the restart path
  (`fallback`, `fallbackReason`), since the round would never go live otherwise. `restart` queues the cvars and
  `mp_restartgame 1` and waits for roundRestarted against the rules read before the send (so a fast restart
  still counts). `input.clear` first. Commands go through `exec_console_confirmed`; `warnings` lists the
  engine's refusals. `timeoutMs` is one deadline for the confirmation and the round wait.

## TODO (step 4, not started)
- Parked-harness mode: `harness.txt` boot once; per test reset (`soft|round|hard`), `gt_begin_/gt_pass_/gt_fail_<test>`
  checkpoints, bridge `input.*`/`console.exec`/`game.tick` assertions, `vtest_release`, JUnit output. Port
  `run_gametest.sh` + `gametest/runner.py` + `gametest/game.py`. Needs the bridge live (workflow 02/03).
- `movie_demo` suite (needs `DEMO=`): not embedded yet.
- Live verification: `run_testscript {suite:"move_jump", judge:"move_jump"}` should pass with 22 checks in total
  (exit + 17 POC + 4 added) on WineTools (the POC passes 17/17 on the same suite).
