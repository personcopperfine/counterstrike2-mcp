# Acceptance runs

PLAN.md "Acceptance": a Sonnet agent with no parent runs a scoped ti_convoy slice through the MCP
only, in a fresh addon. Each run measures:
- commands or scripts outside the MCP (target 0);
- new tooling gaps (target 0);
- whether it stopped on `report_ready` rather than its own judgement (P3 onward; not applicable
  after P1).

## Harness (every run)

- **Addon:** a fresh `accept_p1`, made the way ti_convoy was: an empty `content/csgo_addons/<addon>/maps/`
  and a `game/csgo_addons/<addon>/addoninfo.txt` (IsTemplate 0, IsPlayable 1). This is harness
  setup and is not counted against the agent (LIVE-P1c.md Part 2).
- **Session:** `TOOL=hammer ADDON=accept_p1 cargo bridge runtime up`, with one long-lived hammer-mcp
  behind `target/orch/mcp_session.py` (start / call / list / stop). Every request and reply is logged
  in `target/orch/mcp_session/transcript.jsonl`.
- **The agent's instructions:** call the MCP only through that driver; read `list --full` (the tool
  descriptions and the server instructions); read no source or repo docs; log
  STEP/ERROR/CONFUSED/GAP/DONE in `target/accept-p1/LOG.md`; never work around a gap.
- **The task:**
  - a 2048×512 road with walls and cover;
  - CT and T spawns;
  - a vehicle-sized mover;
  - save, compile and load;
  - play-test (walk the road, the mover moves);
  - a ship-readiness check.

## Run 1: after P1 (2026-10-26, streamline/agent-ux at 3ec0ab3, bridge m3.16)

**Result: blocked at step 1.** About 13 tool calls, 0 commands outside the MCP, 1 GAP.

- **GAP: no tool creates a map.** `open_map` needs an existing `.vmap` (`AssetNotFound
  missing_host_file`). `create_block` and `save_map` answer `NotImplemented`, because hammer.dll only
  loads once a document is open. So an empty addon has no way in, and the whole build, save,
  compile and play chain depends on it. P5's `new_map_from_template` belonged in P1.
- **CONFUSED:** the open_map description mentions the first-map vtm route but not where the first
  file comes from.
- **CONFUSED:** the driver's `list` did not print the server instructions. This was a harness bug,
  fixed in mcp_session.py after the run.
- **Missing pointer:** `session_status` gave no next step for "no document, empty addon".
- **What the agent asked for:**
  - a `new_map` tool or `open_map {create:true}`;
  - a `session_status` hint;
  - a way to list the addon's files;
  - the instructions printed by `list`.
- **Positive:** the agent did not work around the gap, and judged the descriptions of
  `apply_layout`, `play_test`, `tie_to_entity` and `create_trigger` fit for the task.

**Follow-up:**
- the new_map tool (branch streamline/new-map), with the `session_status` and `open_map` next steps
  pointing at it;
- the harness fix to `list`;
- then run 2.

## Run 2: after P1 + new_map (2026-10-26, streamline/agent-ux at 44f5828, bridge m3.16)

**Result: the map was built, saved, compiled, loaded and walked through the MCP alone.** 0 commands
outside the MCP. 2 GAPs. The transcript counts 2073 tool calls: 2019 of them are `compile_map
{jobId}` polls and about 54 are real work. The agent itself reported "about 800" polls, which is
wrong.

Task points:
1. **Blockout: done.** One `apply_layout` (road, walls, end caps, 4 cover blocks), after
   `delete_nodes` removed the template's meshes and gameplay entities (81 nodes).
2. **Spawns: done.** 5 CT and 5 T spawns, checked in the saved file.
3. **Mover: done.** `func_movelinear` (192×96×96, 1400 u at speed 150) tied with `tie_to_entity`,
   plus a `logic_eventlistener` on `round_freeze_end` that sends it `Open`
   (`add_entity_connections`).
4. **Save, compile, load: done.** `compile_map` novis took 50 s; the full build gave "14 compiled, 0
   failed". `run_map` loaded both.
5. **Play-test:**
   - The walk was done: the player reached x=1914, alive.
   - The mover's motion is **only inferred.** The player was stopped at x=1683.97, the mover's
     final face minus the player radius. No tool reads a game entity's position.
6. **Ship check: assembled by hand** from the full compile, `run_map`, `list_entities` and
   `session_status`. There is no `report_ready` yet; that is P3.

Findings:
- **The polling storm (worst UX bug of the run).** `compile_map {jobId}` / `job_status` have no
  wait. The agent was told never to sleep, so it busy-polled about 2000 times.
- **GAP: no game-side entity read in hammer-mcp.** `find_entities_live` reads the Hammer document,
  and `game_state` only reads the local player.
- **GAP: no ship check** (`report_ready`, P3).
- **CONFUSED: lost input.** A `play_test` input step right after a respawn did nothing for 8 s;
  an identical later step worked. Seen twice, cause unknown.
- **Friction: dead on team 0.** The player starts dead on team 0, and the agent needed
  `console_exec "jointeam 3 1"` to play.
- **Friction: template cleanup.** The `new_map` template's meshes and spawns sat in the way; the
  agent deleted 81 nodes by hand.
- **Agent's own mistake:** cover blocks placed in the spawn lane.

Follow-ups go to live-fix D.

## Run 3: after live-fix D (2026-10-26, streamline/agent-ux at 154525a, bridge m3.16)

**Result: P1 acceptance passed.** All six task points done. 0 commands outside the MCP, 0 GAPs. The
transcript counts 79 tool calls by the agent: 9 `compile_map` (run 2: 2019), 14 `play_test`,
9 `entity_find`, 10 `run_map`, 12 `game_state`.

1. **Blockout, done:** road 2048×512, floor, two 256 walls, end caps, 3 cover blocks. Built with one
   `apply_layout` after `new_map`.
2. **Spawns, done:** 2 CT spawns at x=64 and 2 T spawns at x=1984. `run_map {team}` placed the player
   alive on both teams.
3. **Mover, done:** `func_movelinear` (192×96×96, 1300 u at speed 200), fired by a
   `logic_eventlistener` on `round_freeze_end`. **Measured live with `entity_find`:** x 496 → 1796.
4. **Save, compile, load, done:** `save_map` (0 unsaved edits after), a full compile ("17 compiled, 0
   failed") and `run_map`. The compile waits use `waitMs` (no polling storm).
5. **Play-test, done:** a passing `play_test` in which the CT player walked x=64 → 1956 alive while
   the mover moved.
6. **Ship check:** still assembled by hand (compile result, load, `list_entities`). `report_ready`
   is P3. The only load warning is the nav-mesh "re-export" one.

Findings (P2-P4 material, none blocks P1):
- **CONFUSED: round-start timing.** `logic_auto OnMapSpawn` fires at load, and
  `OnMultiNewRound ... Open (delay 20)` never moved the mover. The agent found
  `logic_eventlistener round_freeze_end` itself, at the cost of two extra compile cycles. This
  belongs in a playbook (P4 `blockout_map`/`iterate_loop`) or the entity-class notes.
- **Freeze time not visible in hammer-mcp.** `game_rules` is a client-mcp tool, and `play_test`'s
  `lockWaitMs` default of 20 s was shorter than the freeze. Options: `game_rules` in hammer-mcp (a
  shared play tool, like `entity_find`), or a `lockWaitMs` default taken from the round's freeze.
- **Wedged player.** A player walking head-on into the parked mover got wedged and needed a
  `run_map` reload. The spawns were in the mover's swept lane (the agent's layout choice).
  `report_ready` could warn when a spawn lies inside a mover's sweep.
- **No ship-check tool** (`report_ready`, P3).

**P1 acceptance: met.**

## Run 4: after P3 (2026-10-27, streamline/agent-ux at 159b273, bridge m3.16)

**Result: P3 acceptance passed.** All six task points done. 0 commands outside the MCP, 0 GAPs,
1 ERROR. The transcript counts **25 tool calls** (run 3: 79), 2 of them `compile_map` waits
(run 2: 2019). The agent ran `report_ready` and its DONE line cites it ("report_ready 0 fails"),
so it stopped on the tool's verdict, not only its own judgement. Wall time about 3 minutes.

1. **Blockout, done:** one `new_map {clear:"gameplay"}` (template cleanup in the same call, run 2's
   81-node delete gone), then one `apply_layout` (floor, walls, end caps, 4 cover, mover mesh, 4 spawns,
   listener). It deleted a roof block it had over-built.
2. **Spawns, done:** 2 CT at x=64, 2 T at x=1984.
3. **Mover, done:** `func_movelinear` `convoy_car` (192x96x96, 1500 u at 200) via `tie_to_entity`,
   `logic_eventlistener` `round_freeze_end` -> `Open` (the agent planned this from
   `get_entity_class` and the instructions' round-start trap; run 3 lost two compile cycles here).
4. **Save, compile, load, done:** `save_map`, full compile 47 s ("17 compiled, 0 failed"), `run_map
   {team:"ct"}` alive.
5. **Play-test, done:** one passing `play_test`: the player walked x 64 -> 748; `entity_find` read the
   mover at x 256 -> 862 after freeze.
6. **Ship check, done with `report_ready`:** map profile `warn` (saved, compiled, materials incl. 3
   textures, spawns, spawn_in_mover_path pass; objectives warn). Release profile `incomplete`
   (no_metamod_load skipped by design, test_cvars warn sv_cheats 1, loose_compiled and
   debug_patterns pass). Checked by hand: the mover-lane pass is right (sweep x 160..1852, spawns at
   64 and 1984).
- **`look`:** an editor `tour` of `"map"` before compiling and an engine `player_eye` after loading.
  Both images checked by the orchestrator: real renders of the map, not blank.

Findings:
- **ERROR: `compile_map {waitMs}` without a jobId is refused** ("waitMs waits for an existing job").
  The agent wanted start-and-wait in one call. Fix: accept `waitMs` on start.
- **CONFUSED: `objectives` warns on a map that needs none** (the task asked for none). It keeps the map
  profile at `warn` forever for a custom-mode or blockout map.
- **`look {target:"map"}` frames loosely:** the bounds include the template entities `new_map` keeps
  (minimap boundary, team intro, ...), so the road fills about a third of the frame.
- `play_test` input waits out freeze time (20 s); documented, noted by the agent, not a problem.
- `run_map` called twice by mistake: a harmless reload.
- Agent's wishes: one helper for "mover + round-start trigger" (3 tools today), `apply_layout` tying
  meshes to a brush entity directly.

**P3 acceptance: met.** Evidence: `target/accept-p4/` (LOG.md), transcript
`target/orch/mcp_session/transcript.jsonl` in the main checkout (copy in `target/accept-p4/`).
