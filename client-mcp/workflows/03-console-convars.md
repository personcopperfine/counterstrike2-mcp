# 03 — Console and convars

## Goal
Read the console reliably, issue commands through the right route for the session flavour, and
snapshot/restore convars for deterministic tests.

## Prerequisites
- 01 (runtime paths: `console.log`, `exec`). Read `../idabins/worknotes/vtest/README.md` "Rules
  learned the hard way", `bridge-protocol.md` §7 `console.exec`, `../idabins/docs/data/vconsole.md`
  (VConsole TCP protocol, `-vconport`), `../idabins/worknotes/vconsole-scratch.md`.

## Steps
1. **`console_tail {lines}` / `console_wait {regex, timeoutMs}`** over the host `console.log`
   (`-condebug -conclearlog`; tail-follow with inode rollover; `console.log.prev` rotation as
   `run_vtest.sh`). Note: `echo` output does not reach `console.log` in `-tools`; use
   `testscript_debug 1` + checkpoints as markers.
2. **Command routes**, chosen by `session_status`:
   - **Test script** (any flavour): commands are lines of the staged suite — static, planned ahead (04).
   - **VToolMessage `command`** (WineTools only, no bridge needed): `vtmsend.exe` with
     `_message="command" _data="<cmd>"` (what `vcmd.sh` does to release a parked script).
   - **Bridge `console.exec {command}`** (WineTools + bridge + `-testscript` session; `-32011`
     otherwise): async cbuf queue; **never during a level load** (crashed the server).
   - **VConsole** (`-vconport`, `vconcomm.dll` / Linux equivalent): the engine's own TCP console —
     a route that could work on **LinuxClient** without a bridge; protocol partially documented.
   Tool `console_exec {command}` picks the best available route and returns `{route, queued}`;
   it refuses `quit|exit|disconnect|Test_ExitProcess|crash` (use `stop_session`).
3. **Convars**: `convar_get {names}` (parse `console.log` after `cvarlist`/`<name>` echo; or
   VConsole's convar messages), `convar_set {name, value}` via `console_exec` with `sv_cheats`
   awareness, `convar_snapshot`/`convar_restore` for a test's duration. Standard test preamble
   (from `setup_player.vtest`): `sv_cheats 1`, `mp_warmup_end`, `mp_freezetime 0`, `mp_team_intro_time 0`
   (else the intro camera eats input), `bot_kick`, `tv_enable 1` before `map` when a demo is wanted.
4. **Level-load guard**: track `map_load`/`FinishedMapLoad` checkpoints and `ss_loading`-like log
   lines; `console_exec` returns `EngineBusy` while loading.
5. Tests: log-rotation and regex-wait unit tests; route selection matrix per flavour (mock).

## Definition of done
- On a parked WineTools session: `console_exec getpos` + `console_wait "setpos"` returns the
  position within one tick budget; on a LinuxClient session the tool reports the available route
  (VConsole if proven, else "testscript-only").

## Open questions
- VConsole on Linux as the bridge-less command channel: which messages (`CMND`, `PRNT`, `CVAR`?)
  — finish `vconsole.md` before relying on it; confirm `-vconport` is honoured without `-tools`.
- `console.exec` outside `-testscript` sessions is blocked by design (bridge gate); is a
  `-testscript_inline "testscript_debug 1"` enough to unlock it safely for every session?

## Evidence
- `console.log` judging, `vcmd.sh`, `console.exec` `[V runtime]`, level-load crash [observed] —
  `worknotes/vtest/README.md`, `bridge-protocol.md` §7.
