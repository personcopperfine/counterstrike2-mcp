# Agent streamlining: plan (approved 2026-10-26)

Goal: a weaker model (a "dumber" agent, no parent agent steering it) can finish a Hammer/CS2 task
using only the MCP servers, and the tools tell it when it is done.

Two inputs:
- `ti_convoy-FIXME.md` (copy of `mapcore/ti-convoy:gamemodes/ti_convoy/FIXME.md`, 35 items): every
  time the ti_convoy agent had to leave the MCP. Each such exit is a dead end for an agent with no
  parent. **This is the primary evidence.**
- `research/source2-mcp-research.md` + `research/vdc-deep-dive.md`: an outside agent's survey of
  Blender/Unreal/Unity/Houdini/Source 1 MCP servers and the Valve wiki. Good on *tool shape for
  agents*; its tool catalogue assumes an offline file editor with no editor access, so about half of
  it does not apply (we have the in-process bridge). Its format claims marked UNVERIFIED stay
  unverified until checked against the install / IDA.

## Measured baseline (main df1d3cc, `target/release/hammer-mcp` tools/list)

- 80 tools; descriptions 32,980 chars (median 296, max `run_map` 1527, `create_sound_entities`
  1417, `drain_sounds` 1263), input schemas 59,023 chars: ~23k tokens before the first call.
  Descriptions carry implementation history (review IDs, retired caches).
- `Mutability` (ReadOnly/Mutating/Writer, `crates/cs2-mcp-common/src/tools.rs:26`) is dropped by
  `tool_list` (`crates/cs2-mcp-common/src/mcp.rs:81`): no MCP `readOnlyHint`/`destructiveHint`.
- No MCP prompts, no resources. ~5 ad-hoc `hint` fields, no uniform next step. Errors already carry
  `retrySafe`/`outcomeUnknown` (keep).
- No verdict tool, no multi-angle look, no materials tools, no general asset search (`list_models`).
- `INSTRUCTIONS` (`hammer-mcp/src/lib.rs:25`) describes mechanics, not traps or "done".

## FIXME items by cause

| Cause | Items | Fix in |
|---|---|---|
| Session plumbing blocked the agent (addon fixed at start, single bridge client, raw RPC, hand-pasted layout) | 1, 2, 7, 13 | P1 |
| A tool rules out what the work needs (no console tool, no cvar read) | 3, 4, 20 | P1 |
| Wrong verdicts (good compile reported failed; dirty flag stuck after save -> `UnsavedChanges` forever) | 14, 25 | P0 |
| Tool doesn't cover the case (no-VIS compile, input release+tap, no UV control, particle children) | 15, 26, 29, 31 | P0/P1 |
| Play-testing has no tools (8+ throwaway drivers, no client-view capture) | 22, 27, 28, 34 | P1/P3 |
| Missing content knowledge (magenta materials, model facing, seats by eye) | 12, 21 | P5 |
| Release steps nobody tracks (debug flags, cheats, Metamod-free check, loose particles) | 5, 6, 10, 11, 17, 31, 35 | P3 |
| Gameplay/design | 23, 24, 30, 32, 33 | out of scope |

## Decisions taken (user said "use best judgement")

- **Console tool: yes**, reversing "hammer-mcp forbids a console tool by design". Guard it like
  client-mcp's `console_exec` (refuses quit/exit/disconnect/Test_ExitProcess/crash, plus `maps`,
  which crashes the engine, 2026-10-07) and return the console lines it produced. Without one a
  weak agent writes its own unguarded `tc.py`.
- **No multi-client bridge in this pass.** The single slot only mattered because the addon was
  wrong and tools were missing; addon-follows-document + the P1 tools remove the reason. Revisit if
  the acceptance run still needs raw RPC.
- **Renames/merges keep the old name as an alias for one release** (ti_convoy and its
  THREAD_BRIEF use the current names).
- **No `confirm:true` gate on every mutator** (undo groups + no auto-save already cover it);
  `dry_run` only on compound tools.
- **No wholesale Blender-style renames.** Take `look` and `get_scene_info` where they replace
  several tools; keep domain names elsewhere.
- **Not doing**: VConsole 29000/29009 relay, Source2Viewer as the vision backend, offline `.vmap`
  editing through dmxconvert. We have the bridge, the vtm command channel and in-engine renders.

## Work packages

Each WP: own Sonnet implementer (non-RE) in its own worktree off `streamline/agent-ux`, brief in
`<worktree>/target/BRIEF.md`, the negative test it must show, Opus (or Fable) review before merge.

### P0: land what exists, stop wrong verdicts (first)

- **WP0.1** Bring `6f0db76` (ti-convoy branch: `game_local`, `input_set`/`input_clear`,
  `compile_map` mode `novis`, the OK-line `9m:35s` parse fix) onto this branch as its own commit
  (cherry-pick; resolve against main). Drop its "console_command stays forbidden" wording.
  FIXME 14, 15, 20.
- **WP0.2** FIXME 25: Hammer's dirty flag stays set after `save_map` (`skippedClean:true`) and
  `compile_map` refuses `UnsavedChanges` forever. Find the root cause first (is the bridge's dirty
  read wrong, or does Hammer really keep it?). If it needs IDA, that is RE work: Opus agent per
  `workflows/re-brief.md`. **Root cause found (2026-10-26, IDA, `target/orch/streamline/wp02-re.md`):**
  `ChangedSinceSave` returns `bool` in AL with the upper EAX bits holding a pointer / the 64-bit
  saved mark; the bridge read the whole `i32`, so after >= 256 edits `doc.active` said dirty for a
  clean document. Fixed in the bridge (read `u8`), no compile_map workaround. Negative test: a
  document edited after the save is still refused (existing `compile_refusal` test).
- Rule from here: **a tool that refuses or fails something that worked is a P0 bug.**

### P1: no reason to leave the MCP

- **WP1.1** Addon follows the active document, or is switchable at runtime (FIXME 1, 2, 7). One
  server serves any addon under the content root; `c26600d` only names the other addon today.
- **WP1.2** `apply_layout(file)` / batch `create_meshes` from a JSON file, one undo group (FIXME 13).
- **WP1.3** Guarded `console_exec` for the tools session with readback (vtm command channel +
  `console.log` offset), and a bridge `cvar.get` (FIXME 3, 4). `cvar.get` is new bridge surface:
  needs its pin/SDK path per AGENTS.md "Who owns pointers" (RE agent if a new pin is needed).
- **WP1.4** `compile_map` takes a stage list (`world, phys, vis, bakelighting, nav, …`) beside the
  presets (FIXME 15); `particle_compile` gets `recursive`/folder input that also builds
  `m_Children` (FIXME 31).
- **WP1.5** `align_texture` (world/fit/shift/scale/rotate on faces) (FIXME 29). Bridge work.
- **WP1.6** Play-test tools shared in `cs2-mcp-common` so hammer-mcp and client-mcp expose the same
  names (`input_sequence`, `game_state`, `console_exec`); fix release+tap in one call (FIXME 26);
  a `run_testscript`-style tool replaces `drive.py`/`endtest.py`/`hullshot.py` (FIXME 22, 27, 28).

### P2: tools an agent can read (mechanical, can run beside P1)

- **WP2.1** Pass `Mutability` through as MCP annotations (both servers). Result envelope gains
  `next` (the tool to call next and why); errors gain `didYouMean` / `useInstead` (make the existing
  `ModelNotFound` suggestions the standard shape).
- **WP2.2** Description pass: what / NOT for / units / next. History moves to `hammer-mcp/AGENT.md`.
  Target: halve the tools/list size; check with a before/after tools/list dump.
- **WP2.3** Merge live/saved twins (`get_entity`+`get_entity_live`, `find_entities`+
  `find_entities_live`) with `source: live|saved` (default live); `get_scene_info(query, fields,
  limit)` in front of the `list_*` family. Old names alias.
- **WP2.4** Rewrite `INSTRUCTIONS`: what it's for, first call, traps, intent gating (question ->
  change nothing), definition of done.

### P3: seeing and done

- **WP3.1** `look(target, mode: viewport|angles|tour|frames|player_eye, max_size)` over the
  existing render tools; restores camera state. `player_eye` from a pose covers FIXME 34.
- **WP3.2** `report_ready(scope, profile)`: pass/warn/fail/skipped per check; never pass when
  nothing was checked. `map`: missing/magenta materials (12), spawns, objectives, stale vpk.
  `release`: loose `_c` outside the vpk (31), configurable debug-flag/cheat-command list (10, 17),
  a load without Metamod (6), test cvars (5).

### P4: playbooks as MCP prompts (from what ti_convoy actually did)

`blockout_map`, `iterate_loop` (edit, save, compile stages, run_map, play-test, look),
`cs_script` (build TS, `compile_asset` the vjs, reload; replaces `sync.sh`, FIXME 9),
`particle_set`, `release`. Each ends: done = compile clean + `report_ready` pass + a look described
in writing; "if you had to step outside these tools, log it in the project FIXME and stop."

### P5: content knowledge

`find_assets` (materials too, with compiled status; FIXME 12); `get_model_info` adds bounds, a
forward axis and attachments (FIXME 21); `new_map_from_template`.

## Acceptance

After P1 and again after P3: a Sonnet (or flash) agent, no parent, from a P4 playbook, runs a
scoped ti_convoy slice (blockout, a scripted mover, compile, play-test, release check) in a fresh
addon. Measure: commands/scripts outside the MCP (target 0), new tooling FIXME entries (target 0),
whether it stopped on `report_ready` instead of its own judgement. Record in
`workflows/streamline/ACCEPTANCE.md`.

## Final deliverable

`workflows/streamline/TOOLS_REPORT.md`: every tool both servers expose at the end (name, kind,
annotations, one-line purpose, args, what it replaced/aliases, status), with the before/after
tools/list sizes. Written by a subagent from a live tools/list dump, not from memory.
