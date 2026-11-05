# Handoff: agent streamlining (branch `streamline/agent-ux`)

Written 2026-10-26 by the planning session. Read this, then `PLAN.md` (same folder), then the root
`AGENTS.md` and `hammer-mcp/AGENT.md`. Don't re-derive what is below.

## Where things are

- Branch `streamline/agent-ux` off `main` df1d3cc; worktree
  `<repo>/.claude/worktrees/streamline`. Work here and in
  sub-worktrees off this branch; never in the main checkout (shared, other sessions use it).
- `workflows/streamline/`: `PLAN.md` (approved), `ti_convoy-FIXME.md` (primary evidence),
  `research/` (outside survey; treat its UNVERIFIED claims as unverified).
- `mapcore/ti-convoy` (8b50309, worktree `.claude/worktrees/ti-convoy`) is someone's map branch:
  read it, don't commit to it. Its commit `6f0db76` holds hammer-mcp changes for WP0.1.
- Main has an uncommitted `TODO.md` edit (pins-onto-SDK section 7). Not ours; leave it.

## Established facts (don't rediscover)

- hammer-mcp baseline: 80 tools, ~33 KB descriptions + ~59 KB schemas (~23k tokens). Measure with
  `initialize` + `tools/list` over stdio to `target/release/hammer-mcp` (a python one-liner on the
  JSON). Keep that dump as the before/after reference.
- `Mutability` exists but isn't emitted (`crates/cs2-mcp-common/src/tools.rs:26`,
  `crates/cs2-mcp-common/src/mcp.rs:81` `tool_list`). Server enables tools only (no prompts).
- Errors: `ToolError {type, message, data, retrySafe, outcomeUnknown}`; keep that shape, extend it.
- client-mcp `console_exec` already has the refusal list to copy (`client-mcp/src/lib.rs`,
  `client-mcp/src/tools/client.rs`, `client-mcp/src/console/`).
- hammer-mcp `run_map` already posts console lines over the vtm command channel
  (`cs2_mcp_common::runtime::docker::{vtm_command_payload, vtmsend_program_argv}`) and watches
  `game/csgo/console.log`: WP1.3 builds on that. `maps` crashes the engine (2026-10-07). Whether the
  channel splits on `;` is unverified.
- Bridge `console.exec` is gated on a `-testscript` session (not the tools runtime).
- FIXME 25 symptom: after `save_map` (`skippedClean:true`), Hammer's dirty flag stays set and
  `compile_map` refuses `UnsavedChanges`. Root cause NOT investigated yet.

## How to work (orchestration)

- **Implementers: Claude Sonnet subagents** (Agent tool, `model: "sonnet"`, `run_in_background:
  false` when you wait), for all non-reverse-engineering work. Not the opencode-go flash workers
  this time.
- **Reverse engineering (IDA, new pins/offsets): an Opus agent**, front-loaded from
  `workflows/re-brief.md`. Normal pointer rules: a new slot/offset is a header declaration first,
  never hand-written (AGENTS.md "Who owns pointers").
- **Review: Opus (or Fable)** on every diff before you merge it into `streamline/agent-ux`.
- Each implementer: its own worktree off `streamline/agent-ux`, brief saved to
  `<worktree>/target/BRIEF.md`, names the negative test it must show, forbids new
  skip-if-missing paths. Verify clippy/tests yourself before merging.
- Order: P0 first (WP0.1, then WP0.2), then P1 and P2 in parallel, then P3, P4, P5. Run the
  acceptance check after P1 and after P3.

## Rules that bite here

- **OOM guard**: the host has 30 GB, no swap. Before any workspace cargo build/test: game
  containers down (`podman ps` empty), `free -g` > 20 GB available, `-j2`. Worker cargo runs as
  `(ulimit -v 6000000; timeout 900 cargo … -j4)` with `--test-threads=4`. Max ~2 parallel workers
  while the game runs. Build targets under `target/<name>/`, never `/tmp` (16 GB tmpfs).
- Podman needs `XDG_RUNTIME_DIR=<project root>/.podman/xdg`. At most ~3 game
  containers; one ModelDoc instance only (`TOOL=modeldoc bridge/runtime.sh up`, never replace one).
- Wait on signals with deadlines, never fixed sleeps.
- Generated files are never hand-edited (AGENTS.md rule 1). Valve binaries never go into git.
- No push, PR or issue without the user. Commit only files you changed. Log every hack/throwaway
  helper in `workflows/streamline/FIXME.md`.
- Live tests with a worktree-built binary: `--project-root` / `--vk-layer-dir` (see memory
  "worktree binary live test"); don't overwrite the main checkout's installed bridge DLL without a
  backup.
- The checks in AGENTS.md "Checks that must stay green" must pass before a merge.

## When finished

1. Spawn a subagent to write `workflows/streamline/TOOLS_REPORT.md`: every tool both servers expose
   (from a live `tools/list` dump of the final builds, not memory), with name, kind/annotations,
   one-line purpose, args, alias/replaced-by, status, and the before/after sizes.
2. Report to the user: what landed (commits), what was verified live vs only tested, what was
   skipped and why, open items. Don't merge to `main` without asking.

## Status 2026-10-26 (end of session; resume here)

Tip `streamline/agent-ux` = 5c52797 (+ this handoff commit). Game containers: none up.

- **Done:** P0; all of P1 (WP1.1-1.6, live-fixes A-D, `new_map`): accepted in run 3
  (ACCEPTANCE.md: 6/6, 0 gaps, 79 calls). Play follow-ups (`game_rules` in hammer, freeze-aware
  `lockWaitMs`). WP2.3 (`source live|saved`, `get_scene_info`, aliases listed). WP2.2 descriptions:
  hammer tools/list 127.6 KB -> 80.7 KB (guard `hammer-mcp/tests/tools_list_budget.rs`, cap 81_500),
  client 67.4 KB -> 31.6 KB (guard `client-mcp/tests/tool_list_size.rs`, cap 32_000); annotations
  omit MCP defaults.
- **Bridge installed:** m3.16. Backups in the main checkout's `target/bridge/backup-2026-10-26-p1`
  (m3.12) and `-p1b` (m3.15).
- **Unresolved test flake:** after the WP2.2 merge one `cargo test --workspace` run had 1 failing
  test binary; the immediate rerun was clean and the failing test was not identified. Likely a
  load-sensitive timing test (cf. `run_map::a_default_watch_fits_the_budget_without_a_clamp_note`,
  whose lower bound was replaced by a no-overrun bound in 5c52797). Next session: loop the
  workspace tests a few times, find it, fix or log it in FIXME.md.
- **Next, in order:**
  1. WP2.4: rewrite the server INSTRUCTIONS for both servers (purpose, first call, traps, intent
     gating, definition of done). Agents only read tools/list + instructions, never AGENT.md.
  2. P3: WP3.1 `look`; WP3.2 `report_ready` (spawn inside a mover sweep, missing materials,
     release checks). Then acceptance run 4 (same harness as ACCEPTANCE.md; count calls from
     the transcript, not the agent's report).
  3. P4 playbooks as MCP prompts (blockout_map, iterate_loop incl. round-start timing:
     `logic_eventlistener round_freeze_end`, cs_script, particle_set, release).
  4. P5: find_assets, get_model_info bounds/forward/attachments.
  5. Final: TOOLS_REPORT.md subagent, honest report, ask before merging to main.
- **Open items:** hammer tools/list 80.7 KB vs 47 KB target (bare schema structure ~41.5 KB, needs
  schema restructuring); OpenUnconfirmed/OpenPending untested live; no tab switch; no
  cross-addon open_map; a second hammer-mcp (main-checkout release build) can hold the bridge's one
  client slot -> Busy. Details in FIXME.md.
- **Harness:** `target/orch/mcp_session.py` (main checkout, untracked), addon `accept_p1`,
  transcripts in `target/orch/mcp_session/`.
- **Worktrees** sl-* under `.claude/worktrees/` are merged; they can be removed (game-bins there are
  symlinks to this worktree's `target/`).
- **Update hold:** touch `client/.cache/.cs2-docker-date` again before 2026-11-02.

## Status 2026-10-27 (all phases done; waiting on the user)

Tip `streamline/agent-ux` = 4f590dd (+ this commit). Game containers: none up. Not merged to main,
nothing pushed.
- WP2.4 instructions, P3 (`look`, `report_ready`; acceptance run 4: 6/6, 0 gaps, 25 calls, stopped on
  `report_ready`), run-4 fixes, P4 (5 playbooks as MCP prompts + `get_playbook`), P5 (`find_assets`,
  model `facing`), a P5 live smoke and TOOLS_REPORT.md are merged. All AGENTS.md checks green on
  4f590dd (clippy, 3664 tests, win64 check + clippy, gen.py --check with
  `S2SDK_XWIN=<main>/target/xwin`, sdkcheck 289 pass / 20 skip, sigcheck validate + resolve --strict,
  build-rs.sh).
- Open: hammer tools/list 86.0 KB vs the 47 KB target (needs schema restructuring); no agent has
  followed a playbook live (an acceptance run 5 on a different task would test P4); client-mcp tools not
  re-run live; cs_script still needs the user to build/place the script (no `js` input in
  compile_asset: bridge change + live check); the workspace flake was not reproduced.
- Update hold: touch `client/.cache/.cs2-docker-date` before 2026-11-02.

## Status 2026-10-27 (later): cs_script merged

- `cs_script` tool (init/add/write/read/update/build/compile/status) on the Source2ZE create-cs-script
  2.23.1 boilerplate, project in `content/csgo_addons/<addon>/cs_script/`, output `maps/scripts/`; types
  checked against the game's `point_script.d.ts` (npm 2.23.3 is ahead of our held game build, so a fresh
  init uses the game's file as an override). Bridge m3.17 (`.js` job input) built, NOT installed.
- User said to skip live checks: no bridge install, no acceptance run 5. Unverified live: reload marker,
  `.js` -> `.vjs_c` compile, `build` with a map running. Merge to main still needs the user's go.
- **Next (user, 2026-10-27):** install & test, then list every tool call available.
  1. Game down -> `bridge/install-rs.sh status`; back up the installed m3.16 to the main checkout's
     `target/bridge/backup-2026-10-27-m316/`; `bridge/install-rs.sh install` (m3.17).
  2. `TOOL=hammer ADDON=accept_p4 bridge/runtime.sh up`; `target/orch/mcp_session.py start
     target/debug/hammer-mcp --project-root <project root> --addon accept_p4`.
  3. Live: `cs_script init` -> write -> build (with `run_map` loaded: reload marker) -> place point_script
     (`create_entities`, keyvalue cs_script=maps/scripts/main.vjs) -> save/compile_map/run_map -> console
     marker -> `cs_script compile` (.vjs_c path) -> `report_ready {profile:"release"}`.
  4. List all tools: `mcp_session.py list` (hammer, 101 tools incl. aliases) + a client-mcp dump
     (`workflows/streamline/toolsdump.py`); refresh TOOLS_REPORT.md if needed.
  5. Then ask about merging to main.

## Status 2026-10-28: m3.17 installed, cs_script live-verified

- Bridge m3.17 built and installed in the real tools install (m3.16 backup:
  main checkout `target/bridge/backup-2026-10-28-m316/`); `session_status` showed `bridgeVersion m3.17` live.
- Live on accept_p4 (convoy_slice): `cs_script` init (npm 4.4 s, type drift -> override), write, a broken
  script (TS2339/TS2322 at file:line:col), build, point_script placed + save + compile_map (16 ok, 0 failed),
  run_map ct, `[cs_script] [main] loaded v1` + `activated` in game, hot reload v2/v3 `reloaded:true`,
  compile -> `.vjs_c`, `report_ready {profile:"release"}` incomplete only for `no_metamod_load` (2 expected warns).
- Fixed from the run (FIXME "cs_script" + "Live test 2026-10-28"): the engine loads only the `.vjs_c` (build now
  points at compile BEFORE run_map; playbook step 3), real reload markers, and unknown argument names are
  refused for every tool. Checks: clippy 0 warnings, 3689 tests passed, 0 failed.
- Live tools/list: hammer 100 (incl. 2 deprecated aliases), client 46; dumps in the main checkout
  `target/orch/tools-{hammer,client}-live.txt`.
- Game is down. Open: merge `streamline/agent-ux` to main (user's go).
