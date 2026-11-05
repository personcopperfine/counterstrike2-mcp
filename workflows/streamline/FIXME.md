# Streamline FIXME (hacks and unverified assumptions)

## WP1.4

- `particle_compile` folder/children runs pass several `-i` to one `resourcecompiler.exe`
  (podman route). Single `-i` and `-r -i <glob>` are proven live (ti_convoy); several `-i` in one
  run is NOT yet seen live. If the compiler only honours the last `-i`, the others are not
  refreshed and the compiled + skipped count is below the number of inputs (it is checked as "at least": the compiler also counts dependencies), so the run reports
  `ok:false` with reason "compiler did not account for every input" (an up-to-date-by-mtime output
  is only accepted when the count adds up).
- Per-file verdicts on the podman route come from output mtimes plus `[FAIL]` lines that name the
  file; the compiler's single OK summary cannot be split per file.
- Child references outside the addon are resolved addon source, addon `_c`, `content/csgo|core`,
  game VPKs. Whether the compiler itself resolves a stock child the same way is assumed, not seen.
  If the game VPKs cannot be read, such children are passed as `source:"unknown", verified:false`
  with a note (our lookup failing is not the file failing).
- `compile_map` `full` sends no stage flag; the result says so (`stageFlags: []`,
  `stagesFrom: "compiler default"`) while `stages` still lists the five documented ones. Only the
  `nav` and `novis` subsets are live-proven; other `stages` lists are untried combinations of the
  same flags. lod/gridnav/sareverb/sapaths/sacustomdata have no known flag spelling.

## Tests

- `console_exec::a_cut_off_exec_is_a_delivery_warning_and_never_repeated` flaked on a stalled host
  (WP2.2 review) although live-fix A had called it fixed: the fake exec forked bash/cat/cp before
  `sleep 600` and the 2 s knob also bounded the `podman ps`. Fixed in WP2.2 (`exec sleep 600`,
  `--tools-container cs2tools`, server-side assertions instead of counting the fake's payload
  copies); proven under 48 CPU burners (see the WP2.2 report). The run_map budget flake below is
  still open.
- `run_map::a_default_watch_fits_the_budget_without_a_clamp_note`: its lower bound (`elapsed >= budget - 300 ms`) was
  not a real guarantee (the server keeps a margin for its final reads); replaced by a no-overrun bound (WP2.2 merge).
- Unidentified workspace flake: one `cargo test --workspace` run after the WP2.2 merge had one failing
  test binary (not captured). 5 full runs on 2026-10-27 (4 of them beside another `-j2` build) were all
  clean. If it recurs, keep the log (`--no-fail-fast > target/flake/runN.log`) and name the test here.
- WP2.1: `Registry::alias` builds the alias description with `Box::leak` (`ToolSpec` names and
  descriptions are `&'static str`); a one-time, startup-only leak per alias. Remove if ToolSpec moves
  to `Cow<'static, str>`.
- WP2.1: client-mcp still emits string-valued `next` (runtime_tools, multiplayer, testscript,
  game_state, video.rs:537, bridge_tools.rs:385, script.rs:104/159/334/410, runtime_tools.rs:423/661/793/954,
  multiplayer.rs:581, testscript.rs:432, gametest/run.rs:754); they are not the array shape. Convert in
  WP2.2 (gametest/junit.rs reads the string form).
- WP2.1: entity-class errors keep the old `data.nearest` next to the new `didYouMean` for one release.
- WP2.1 follow-ups (review, not done): no test for a successful compile with `sourceChanged:true`
  suggesting `compile_map`; that step passes only `{path}`, so a nav/novis job is re-suggested as a
  full compile (should carry the job's `stages`/`mode`). Classification nits: client `game_snapshot`
  `export` and the render tools write files but stay ReadOnly; `round_start {mode:restart}` could be
  destructive. The save_map step hook runs after the write gate is released, so a concurrent read
  can race `untitled`/the counter (rare: a wrong hint or a missing step).

## WP2.2 (client-mcp)

- Shared input/sequence/game_state/entity_find schema text was shortened once in
  `crates/cs2-mcp-common/src/play/{input,sequence,state,entities}.rs` (the long `press`/`angles`/`turn`
  strings, `holdTicks`, `label`, `steps`; defaults moved into `default` keys). hammer-mcp still has its
  own copy of the `frames` text in `hammer-mcp/src/tools/game_live.rs` and its guard still exempts the
  shared strings: the orchestrator removes that exemption after both WP2.2 branches merge.
- `client_mcp::tools::retext` / `INPUT_RETEXT` (exact-match rewrite of shared descriptions, a hack
  that silently stopped working on any reword) was removed with that change; `input_set` and
  `input_sequence` now describe themselves the same in both servers.
- `cs2-mcp-common::mcp::annotations` now omits MCP-default hints (see its doc comment); clients that
  assumed an explicit `readOnlyHint:false`/`destructiveHint:true` must treat absence as the default.
- Not done (review item 7): `default` keys for game_wait `ticks`/`minDistance`/`pollMs`, render_world
  znear/zfar/height, launch_session `timeoutMs`, game_restore `parts`, server_up `map`.

## WP1.1 addon selection

- The document-derived addon is re-read by `ToolState::before_call` before every tool outside
  `state::ADDON_EXEMPT_TOOLS` (hand-kept: add a tool there only if it never touches the addon or its
  handler reads `doc.active` before the first addon use; `create_sound_entities` still pays the pre-call
  read because its sound verdicts sit inside `plan_entity`).
  Bridge-backed read-only tools keep a 1.5 s throttle; the rest cost one extra `doc.active` per call.
- With no document open the addon falls back to `--addon`; the Hammer tools container's
  `cs2mcp.addon` label is not consulted (review item 5, optional, not done).
- The caches (model catalog, soundevent index) are rebuilt from scratch on an addon change; the game
  VPK halves are not shared across addons yet (a switch costs one full build).
- The ModelDoc addon is read with `podman inspect` per guarded call (`container_addon`); a
  ModelDoc started before `cargo bridge runtime` set the `cs2mcp.addon` label is read from its launch
  command (`-addon <a>`), and one with neither is treated as "unknown" (the guard passes).
- model_edit dry-run: the ModelDoc-on-another-addon warning is worded as if the file was written
  ("the file was written in addon ..."), also for `dryRun:true`.
- create_entities needs an open document before it refuses an unknown sound (the sound check runs
  after `require_document`, so with no document the caller first sees NoDocument).
- model_status / particle_status `inAddon` can be up to 1.5 s stale (they use the throttled
  document-derived addon).
- One extra `doc.active` per pet/bridge mutator: the pre-call check reads it and the handler reads
  it again.
## WP1.5 align_texture / mesh.setFaceTexture

- `align_texture` does not check face indices against the mesh's face count client-side (that would need
  a `mesh.get` round trip per call); the bridge refuses an out-of-range index before writing anything.
- `mesh.get` / `get_mesh` do not report the per-face texture fields (the `mesh.get` reply budget has no
  per-face texture bytes yet). `align_texture` returns the read-back of the faces it touched instead.
- Group justify (justify several faces as one block) is not offered: it needs `sub_1808EA010`, the
  world-extreme-point bounds function, which is not pinned.
- The mock bridge (`cs2-mcp-common` `mock/hammer.rs`) keeps texture values outside its undo snapshot:
  `undo_last` pops the group but does not restore texture values there.
- `rotate`'s sign is unverified against Hammer's RotateTexture90CW; the tool says "sign as Hammer's rotate
  field". Fix after the live check (crates/cs2-bridge/PROTOCOL.md §12.6).
- WP1.5 review: `align_texture`'s refusal classification (retry-safe) matches the bridge's pre-write
  reasons by text ("mesh has no faces", "could not collect face handles") and `TextureSetUnknown`; any new
  pre-write `-32603` reason must be added to `align_refusal` (mesh_live.rs), else it is reported as
  outcome-unknown (the safe side).
- WP1.5: `mesh.get` per-face `texture` (the pure GET read for live verification) is still not done.

## WP1.2 apply_layout

1. **apply_layout undo groups.** The bridge's `edit.batch` only runs `entity.create` and
   `entity.setKeyValues` ops, and `mesh.create` opens its own undo group, so a layout of N meshes
   is N undo groups (+1 for the entity batch) and the requested `name` cannot become the group
   label. The tool reports `undoGroups` and `recovery`. Proper fix: a `mesh.create` arm inside
   `edit.batch` (no group of its own, like `op_entity_create_no_group`), taking an `undoLabel`;
   then a layout is one request and one `undo_last`.
2. **Mesh limit and budget.** `MAX_LAYOUT_MESHES` = 32 is a conservative count cap, not a measured
   time budget (the per-mesh `mesh.create` latency was not measured live). The call budget is also
   checked before every request, and a budget miss mid-way is reported like any other partial
   failure. Proper fix: measure a live layout, then derive the cap from the budget.
3. **One `expectDoc` per layout.** The mesh.create reply carries no `doc` (only edit.batch does), so
   there is no post-write fingerprint to chain. The layout reuses its first `doc.active` read: the
   real fingerprint (`doc_fingerprint`) is structural and should not move on edits. The mock bumps
   the epoch on every mutation, so the layout tests set the new `HammerWorld::edits_keep_epoch` seam
   (crates/cs2-mcp-common/src/mock/hammer.rs, default off); verify live that two consecutive mesh.create calls with one expectDoc succeed.
   Follow-up (Opus re-check): the mock's default is the wrong one. The bridge only moves the epoch in
   `fingerprint_step` (session/root/path/world change), not on edits. Flip the mock default to
   keep the epoch, drop the seam, and see which other tools stop re-reading `doc.active` per step.



## WP1.3 console

1. `cvar.get` (bridge m3.13) is implemented from `workflows/streamline/re/cvar-re.md` and only host-
   tested (a synthetic `ConVarData`); the three engine calls (`FindConVar` hidden return,
   `GetConVarData`, the member reads) have not run against a live tools session. Live check:
   `cvar_get {names:["mp_c4timer","sv_cheats","game_type","nope","sv_password"]}` and compare the
   values with the in-game console; expect `nope` found:false, `sv_password` redacted.
2. The bridge's `-tools` / `-testscript` flag matcher (`cmdline_has_in`) is anchored at the end of
   the token only, so `x-tools` matches too (kept from the C bridge, pinned by its tests). A
   command line that merely contains a path ending in `-tools` would open the console gate. Low
   risk (the gate is a policy, not a security boundary), left as is.
3. `console_exec` ends its wait on a marker command (`Unknown command: cs2mcp_mark_...`) queued
   right after the line, the technique client-mcp uses. Unverified live for the `-tools` session:
   (a) does the bridge route's command output reach console.log at all (client-mcp notes `echo`
   and `status` output never did in `-testscript` sessions)? (b) does the vtm route print the
   marker line? If not, every vtm call runs to `waitMs` (`endedBy: timeout`, with a note) and the
   marker could be dropped from that route. (c) does the vtm channel split on `;` (the marker is a
   separate post, so the marker itself does not depend on it, but a chained user line does)?
4. `console_exec`'s `EngineBusy` check knows loads started by this hammer-mcp process
   (`HammerState::last_load`) and, from the last level line in the console.log tail (256 KB),
   one started by anyone else; with no console.log and no `game.local` it cannot tell and sends.
5. The guard does not read what an `exec <cfg>` file contains (`exec autoexec` can run `quit`);
   `alias` and `bind` bodies are checked. Left as is: a cfg is the operator's own file.

## WP1.6 play-test tools

1. **release+tap (ti_convoy FIXME 26) was the bridge.** `input.set {release:[b], tap:[b]}` of a held
   button cleared and re-set the wanted bit inside one call, so the per-frame body (one delta per
   changed bit) never saw a change. Fixed in the bridge as `regap` (one gap frame; bridge m3.15,
   `crates/cs2-bridge/PROTOCOL.md` §19.1). The scenario was inferred from the FIXME wording ("release + tap
   in one call doesn't release"): same-button release+tap is the only combination that breaks. Test:
   `crates/cs2-bridge/tests/ops_hooks.rs`. Not run live.
2. **m3.15 numbering.** WP1.3 may take m3.13 or also bump; `BRIDGE_BUILD` in `crates/cs2-bridge/src/lib.rs`
   needs merging by hand if both land (doc comment lines too).
3. **`play_test` is hammer-mcp only.** Its `wait_log` uses `run_map`'s `ConsoleWatch` (made
   `pub(crate)`), which reads the tools session's `game/csgo/console.log`. client-mcp has its own
   `ConsoleLog`; registering `play_test` there needs a small adapter over it (the step engine is not
   generic over the log source yet). Also no `console` step: wait for WP1.3's shared guard.
4. **`play_test` holds the bridge lock for the whole call** (like `input_sequence`), so other bridge
   tools queue behind a long run. The call budget bounds it.
5. **`wait_state` polls `game.local` every `pollMs` (min 20)**, not per game tick; a state that
   flickers for less than a poll is missed. `wait_log` polls the file every 50 ms.
6. **`wait_log` `since: step` default** sees only lines printed after the step began (the brief's
   wording). A line printed during an earlier step is missed; `since: start` covers it.
7. **`cs2-mcp-common` now depends on `s2sdk-sys`** (for the entity-index mask in the moved `game_wait`
   predicates). The `game_wait` engine (`play/wait.rs`) moved whole with its tests; client-mcp's
   `game_wait.rs` is the wrapper, the schema and the registration.
8. **client `game_state` stays single-read** (its schema is empty; `samples` is hammer-only for now).


## Live-fix B

Review follow-ups (open): (a) handlers still take `state.bridge.lock().await` unbounded after the
gate; add a bounded `lock_bridge(ctx)` helper. (b) `BridgeBusy` could name the tool holding the lock.
(c) ~~Live check owed: a single 600-1023-byte `console_exec` line~~ Measured in LIVE-P1b: the engine's
limit is 510 bytes per `;` command; see "Live-fix C" item 2.
(d) Redacting by name hides harmless `*token*` cvars and, in a `-tools` session, they cannot be read
any other way; accepted as policy, explained in `redactedNote`.

From `LIVE-P1.md` findings 5-10. Not run live; every item has a mock test.

1. **Long console lines (5).** The engine's own limit is not proven: 1117- and 1135-byte lines
   were dropped, nothing between 1024 and 1116 was tried. So `console_exec` refuses anything over
   the bridge's 1023 bytes on every route (`InvalidArguments`, nothing sent) and the vtm long-line
   fallback is deleted. If a live probe shows the engine takes longer lines over vtm, the limit
   could be raised for that route. A `Command too long` line in the readback is a `deliveryWarning`.
2. **`waitFor` and the vtm echo (6).** The echo block shape is from `07a-console-ingame.txt`:
   header `[ToolFramework2] Handling VToolMessage:`, `{`, `_message`, `_data`, `}`. The mask is
   structural (header, then `{`..`}`), not a regex on the command text. An engine that logs the
   block in another shape would match again.
3. **`echo` / cvar prints on the bridge route (7).** Only documented and steered (a note on `echo`
   and on a first token with an underscore that is the whole line); the cause (those prints do not
   go to console.log under `-tools`) is not investigated.
4. **Secret cvars (8).** Redaction by name lives in the tool layer (`console::redact_cvar_reply`),
   in both servers' `cvar_get`; the bridge still redacts by the `protected` flag only. No protocol
   bump. `console_exec <name>` can still print such a value.
5. **Bridge lock wait (9).** `Registry::call` now bounds the capability gate's wait for the bridge
   lock at `min(10 s, remaining budget)` and answers a retry-safe `BridgeBusy` (nothing sent). This
   applies to every bridge-backed tool in both servers, so a call queued behind a long holder
   (`play_test`, a long `input_sequence`) is refused after 10 s instead of waiting
   for the whole holder. Tools called by other tools in-process via `require_caps` still wait
   unbounded. The addon refusal (`AddonCheckFailed`) after the gate is unchanged.
6. **`input_clear` `held` (10).** It is the tool layer: the bridge replies with the status read at
   the clear, before the next frame applies the release, so `held` can list just-cleared buttons.
   Both servers now answer `held:[]` and put what was listed in `releasedHeld`. A bridge-side fix
   would be a protocol change and was not made.
7. **Quit refusal text (10).** The guard's "use stop_session instead" hint is now a parameter
   (`refusal_hinted`); hammer-mcp passes none (it has no `stop_session` and no tool that ends the
   session), client-mcp keeps `STOP_SESSION_HINT`.
8. **apply_layout dryRun `next` (10).** Carries every argument the dry run had (layout or path,
   name, strict...) with `dryRun:false`. A big inline layout is repeated in `next`.

## Live-fix A

1. **`run_map` only checked that the vpk exists** (LIVE-P1 "Unplanned events": a corrupt vpk FATALs
   the engine and ends the tools session). Now it also reads the 12-byte header (signature
   `0x55AA1234`, version 2, tree size within the file; the shape of three real compiled map vpks
   seen on disk). Deeper corruption (a good header over a damaged tree/body) is still NOT caught
   and still FATALs the engine. Also not seen: a vpk whose header is written but whose body is
   still being written by something other than this server's `compile_map` (a `compile_asset` of a
   `.vmap`, another hammer-mcp, a Hammer GUI build); the `CompileRunning` guard knows only this
   server's `compile_map` jobs. The version-2-only rule is from those three files; a v1 vpk would
   be refused.
2. **Cross-addon `open_map` is not possible.** The bridge's `map.open` accepts only a canonical
   `maps/...vmap` (`map_path.rs` `open_path_ok`) and its host gate derives the addon from the
   engine's own command line (`-addon`), so no proven path form opens a map of another addon.
   `open_map` now prechecks under the engine's addon, read from the tools container's
   `cs2mcp.addon` label (`data.engineAddonSource: "container"`; `--addon` only when the label cannot
   be read, `"flag"`), and refuses a map that exists only in the selected other addon
   (`WrongEngineAddon`). Real cross-addon open (an absolute or `csgo_addons/<addon>/`
   path through OpenFile, or restarting the session on the other addon) is untried.
3. **Compiler skip vs. mtime.** `particle_compile` `skipped` verdicts: the compiler said "skipped"
   with an output older than the source (LIVE-P1 bug 3). We now report `ok:false` with a reason and
   `force:true` (`-f`) as the remedy. Not run live: that `-f` makes the compiler rebuild such a
   file is the documented meaning of the flag (the allowlist already had `-f`), not seen here.
4. **Bridge `-world -phys -vis -bakelighting`** now pass the `job.start` allowlist (m3.16). Not run
   live; the allowlist is pinned against what hammer-mcp emits by a test, but whether the compiler
   accepts every `stages` subset is the same open question as in WP1.4.
5. **Addon-relative `vpcf`/`vmdl` paths.** A relative path whose first segment is not
   `csgo_addons`/`core`/`csgo` is now addon-relative. A relative path that used to mean
   `content/<other dir>/x` (nothing in the tools wrote there) now means `<addon>/<other dir>/x`.
6. `particle_edit`, `model_*` still return a string `nextStep` (only `particle_create` moved to
   the `next` array); the shared registry shape for those is WP2.1's.


## Live-fix C

From `LIVE-P1b.md` "New findings" A-D. Mock tests for 1-3; not run live.

1. **First-map `open_map` (A).** When `map.open` is not advertised (fresh session, hammer.dll not
   loaded) and a tools container runs, `open_map` posts `open_asset maps/<x>.vmap` over the
   VToolMessage channel (the `cargo bridge runtime open` route), then polls the capabilities and `doc.active`
   until the document is the map (`openedVia:"vtm"`). The document is matched by the addon-relative
   path tail, case-insensitive. A timeout is `OpenUnconfirmed` (outcome unknown). Not covered: the
   post's real delivery is only known from the document appearing; a modal that blocks the open
   (a map that parses locally but Hammer rejects) shows up as the same timeout, indistinguishable
   from a slow load. The poll is a fixed 250 ms interval inside the call budget (a re-read of the
   capabilities each time), not an event. **Live check owed:** fresh `cargo bridge runtime up`, then
   `open_map {path:"maps/p1_test.vmap"}` returns `openedVia:"vtm"` and a document.
   Review fixes to item 1: the vtm route needs `modules["hammer.dll"].state` `not_loaded`/`verifying`
   (not just a missing `map.open`); `doc.active`'s `-32017 {module:"hammer.dll"}` maps to
   `HammerNotLoaded` (was `AddonCheckFailed` for any open_map without a `set_addon` pin); the
   post settles an unresolved `run_map` load first (`EngineBusy`); `OpenPending` blocks a blind
   second post for 60 s; 5 s minimum budget, bounded gate wait and probes. **Live check owed:**
   open_map with no `set_addon` pin on a fresh session reaches `openedVia:"vtm"`.
2. **Console limit (B).** Measured limit: 510 bytes per `;`-separated command, 128 bytes for a
   command name; the bridge's 1023-byte line buffer stays as a second cap. Segments are cut like
   the guard cuts them (every `;`, newline and CR, quotes ignored), so a quoted `;` can hide one
   over-long engine command; the readback `deliveryWarning` (now also for `Console command too
   long.`) is the net. The 510 is measured on the trimmed segment; whether the engine counts
   leading whitespace or the NUL is not known (a 510-byte command ran; 511 did not).
   **Live check owed:** a 510-byte command runs, a 129-byte command name is refused locally.
   client-mcp's `console_exec` and `server_console_exec` now run the same check (review fix 9).
   `echo_block_mask` masks the line after `Command too long... ignoring!` only (the live
   `Console command too long.` has no command copy after it).
3. **Error `code` for host-side refusals (C).** Decision: document, no table. `code` is not
   unique per type (bridge wire codes, `-32602` reused by many host-side refusals, `null` for
   others); clients switch on `type`/`name` (both AGENT.md files say so). A host-side
   numbering would be a second contract across dozens of error names, in a range that has to
   avoid the bridge's -32xxx, with no consumer asking for it.
4. **Background-tab `open_map` (D).** The reply says plainly that the map is not the active
   document, names the active one (`activeDocument`), and lists what the existing tools can do:
   read the saved file by path (structured `next`: `list_entities {path}`, `session_status`), or ask the user to switch tabs. **Bridge work, not done:** a
   `doc.activate`/`map.switch` method (activate a session by id or path) plus a tool, and a
   `doc.close`. Until then an agent cannot get back to the first map after opening a second one.

## new_map
1. **No `template:"empty"`.** A world without the template's spawns, light and sky is cheap to
   write with `cs2-dmx` (take the template's `Datamodel`, empty `world.children` and
   `nodeInstanceData`, drop the unreachable elements), and `cs2-dmx` reads it back. But the claim
   "Hammer loads it" is only provable by opening it in Hammer, which no test here can do, and a
   map with no spawns is not playable anyway. Not offered; the default copy of Valve's template is
   the proven start. **Live check owed:** write the pruned file and open it with `open_map`; if it
   loads, add `empty` plus a `cs2-dmx` round-trip test against the real template.
2. **Copy keeps the template's thumbnail and `map_asset_references`.** Hammer rewrites both on the
   first save. Harmless; noted so nobody mistakes the stale thumbnail for a bug. The map also
   references resources that live only in the template ADDON (checked on the real file: 3x
   `postprocess/basic_linear_post.vpost`, the soundscapes `ambient_example.outdoors/.indoors`; no
   `xxx`/`mapname`/`addon_template` text, so no rename is needed). `new_map` copies, never
   overwriting and never writing into `addon_template`: the `.vpost` into `content/` (and its
   compiled `_c` into `game/csgo_addons/<addon>/` when the template addon has one and that dir
   exists), and `soundevents/soundevents_addon.vsndevts` only when the new addon has none (an
   existing one is NOT merged: reported). The `.vsnd` tracks those events reference are not copied
   (silent tracks). Every reference is listed in the result's `templateAssets {copied, missing,
   alreadyPresent}`; a `.vpost` that is not in the template addon is listed as missing (it may be a
   game-VPK resource).
3. **`new_map` in an addon other than the engine's.** `open:true` is refused up front
   (`WrongEngineAddon`) because `map.open` resolves under the engine's `-addon`. `open:false`
   still creates the file; opening it needs a session started on that addon.
4. **Live checks owed.** (a) `new_map {path}` in `accept_p1` (empty, Hammer not loaded) reaches
   `openedVia:"vtm"` and a document with the template's entities (`session_status`, `get_selection`,
   `list_entities`). (b) The copied file is not an "Unrecognized Asset"/modal case. (c) `save_map`
   after one `create_block` writes it. (d) `compile_map` + `run_map` on the new map; **and the
   compile log shows no missing-resource error/warning for `basic_linear_post.vpost`** (the copied
   postprocess resolves), and the two soundscapes resolve after `compile_soundevents`. (e) Case: a
   `new_map` on `maps/Foo.vmap` next to a live `maps/foo.vmap` is refused (`MapExists`, `didYouMean`)
   and the `open_map`/`compile_map`/`run_map` on the wrong case answer `didYouMean`, no `new_map`.
   (f) `new_map overwrite:true` on a map open in a background tab: `existing:true`, `editorStale`.
5. **No other changes to `addon_template`.** The selected addon `addon_template` is refused
   (`InvalidArguments`); nothing ever writes there.

## Live-fix D

Everything here is unit-tested against the mock; **nothing ran live** (no container was up).

1. **Job wait is a poll loop.** `waitMs` on `compile_map {jobId}` / `job_status {jobId}` polls the
   job view every 500 ms inside the call (`jobs::wait_for_job`). The job table has no completion
   signal: bridge jobs end inside the bridge. A bridge-route wait costs one `job.status` per 500 ms
   while it lasts. If the bridge ever pushes job events, wait on those. Default `waitMs` is 30000 (a bare
   `{jobId}` waits; `waitMs:0` answers at once), and the last poll's request deadline is cut to
   the budget left. **Live check owed:** a `novis` compile with
   `compile_map {jobId, waitMs:30000}` returns once with `done:true` (or `done:false` +
   `waitedMs` ~ 30000 and a `next`), not one reply per 30 ms.
2. **`entity_find` is ReadOnly and listed in neither `LIVE_MUTATORS` nor `NOT_DOCUMENT_EDITS`.**
   The brief asked for `NOT_DOCUMENT_EDITS`; that list is tested to equal the set of `Mutating`
   tools, and the tool is a pure read, so it is registered `ReadOnly` (same as client-mcp). It is in
   `ADDON_EXEMPT_TOOLS`. **Live check owed:** `entity_find {class:"func_movelinear"}` in the tools
   session returns the mover with a moving `origin`, and `play_test` `wait_entity` passes on it.
3. **Dead on team 0 (confirmed in the transcript).** Right after `run_map` (call 829-830)
   `game_state` shows `team:0, alive:false, lifeState:2`; `console_exec "jointeam 3 1"` put the
   player on CT (console: `ChangeTeam() ... req team 3`), and only `mp_restartgame 1` (call 833-834)
   made it alive at the spawn. So `run_map {team}` = join, then wait; if the player is on the team
   but dead, one `mp_restartgame 1`. **Assumption, not seen:** whether a join during warmup spawns
   the player at once (then no restart is sent). **Live check owed:** `run_map {team:"ct"}` and
   `run_map {team:"t"}` on `convoy_slice`: `team.alive:true`, and which of them restarted the game.
4. **Lost input after a respawn: hypothesis, not proven (corrected after review).** The earlier
   reading used calls 856 and the "~20 s of polling" after 854; those were sampled AFTER `play_test`
   had cleared input (`inputCleared:true`), so no movement there proves nothing. The real evidence
   is call 847 (and 855): `mp_freezetime 2`, forward pressed about 1 s into the freeze and held
   about 7 s after it ended, origin x stayed 100 the whole time. A press made DURING the lock is
   lost for good. Hypothesis: the bridge sends `+1` button deltas (`ops/input.rs` `apply_delta`),
   and the game drops or clears them in the lock; the held bit is then never asserted again.
   Possible bridge fix (not made, no m3.17): re-assert the owned held bits after a respawn or the
   freeze end. In the tool: `play_test` waits the lock out before a key-pressing input step (at
   most once per run, `lockWaitMs`), and a step still locked at the limit fails without sending
   (`movementLocked`). Keys held from an EARLIER step across a respawn are NOT covered. **Live check
   owed:** press forward during a 2 s freeze, check it stays dead after the freeze (confirms the
   hypothesis), then `play_test` right after `mp_restartgame` passes with a `movementLock` note.
5. **`new_map clear`.** Ids come from the parsed template, not from a live listing, so they are
   right only for a freshly opened copy (an `existing` document skips the clear). The match-flow
   cameras (`team_select`, `end_of_match`, team intros) are KEPT by `gameplay`: they are not gameplay
   obstacles and `defuse_poc` ran with them, but they sit at the template's coordinates. `all`
   removes them. Delete failures partway are reported in `clear.error` with the count already
   deleted (each 256-id request is its own undo group); `clear.removed` counts the chunks that
   succeeded and `clear.planned` what was meant to go. The active document is checked against the
   open before the first chunk (`clear.skipped` on a mismatch). Nodes of other kinds (nav data,
   groups) are listed under `kept`; "every mesh" includes the toolsskybox mesh. **Live check owed:** `new_map
   {clear:"gameplay"}` in `accept_p1` leaves the sky, light, fog and post-processing, `save_map`
   writes it, `compile_map` + `run_map {team:"ct"}` loads it; `undo_last` restores the template.
6. **`new_map::a_pending_first_map_open_refuses_new_map...` flaked once** (`BudgetTooLow`, 3011 ms
   of the 45 s budget left) in a full parallel `--test new_map` run on this machine, and passed three
   times alone: it is timing-sensitive under load, not touched by this work.

## Play follow-ups

From acceptance run 3 (`workflows/streamline/ACCEPTANCE.md` "Run 3").

1. **`game_rules` in hammer-mcp.** Shared through `cs2_mcp_common::play::state::rules` (client-mcp now
   calls the same function, output unchanged); addon-exempt, ReadOnly, `NoMapLoaded` (useInstead
   `run_map`) when no map runs. Whether the bridge answers `game.rules` with `-32024` or `inGame:false`
   with no map is handled both ways but not seen live. **Live check owed:** `game_rules` in hammer-mcp
   after `run_map` shows `freeze`, and before any map answers `NoMapLoaded`.
2. **Freeze-aware `lockWaitMs`.** `game.rules` has no remaining-freeze field (only `roundStartTime`,
   `roundTimeSec`), so the default is sized from `mp_freezetime` via `cvar.get`, read once when a lock
   is first seen: `max(20000, freezetime + 5 s)`, capped at 60000. The effective value and its source
   are in `movementLock.lockWaitMs` / `lockWaitSource`. Not covered: a team intro on top of a
   freeze longer than the 5 s slack, and a freezetime above 55 s. **Live check owed:** a
   `play_test` right after load with `mp_freezetime 30` no longer fails with `movementLocked`.
3. **Round-start knowledge** is a `playNote` on `get_entity_class` / `list_entity_classes` for
   `logic_auto`, `logic_eventlistener` and `func_movelinear`, hard-coded in `entity_classes::play_note`
   (live evidence from run 3, not read from the FGD). Hack-ish: a static table; move it to a data file if
   it grows past a handful of classes.

## WP2.3

1. **Default source flipped to live.** `get_entity`/`find_entities` without `path` used to read the
   saved file of the active document; they now read the live document when the bridge has one open.
   Callers that wanted the file say `source:"saved"`. `sourceUsed`/`sourceWhy` say what happened.
2. **`play/entities.rs:3` comment** now says `find_entities {source:"live"}` (fixed after the play worker
   merged).
3. **Aliases stay listed.** The first cut hid `game_local`, `get_entity_live` and `find_entities_live`
   from `tools/list`; review showed MCP clients only expose listed names, so the "alias for one release"
   promise would have been void. The `mcp.rs` filter and the stdio test changes are reverted; the alias
   description is now `Deprecated: use `X` {args}.` (about 40 B each). **Hide or remove them next
   release** (all three).
4. **`targetname` on `get_entity`:** case-insensitive on BOTH routes (Hammer's semantics; saved was
   case-sensitive) and 2+ matches are `AmbiguousTargetname` with candidates on both. Not live-verified:
   the duplicate-name case through the real bridge (`entity.find` with `limit:2`).
5. **`get_scene_info` live bounds use mesh ORIGINS**, not extents (`node.list` has none); the saved
   route uses the vertex bounds. **Live checks owed:** `get_scene_info` on the open `accept_p1` document
   (counts match `list_entities {classnames:true}` after a `save_map`), `get_entity {nodeID}` and
   `find_entities {class}` with no `source` live in Hammer (`sourceUsed:"live"`), then after closing the
   document with a `path` (`sourceUsed:"saved"`), and `get_entity_live` still answering with `deprecated`.

### WP2.3 review fixes

- **`nameContains` (item 8):** `entity.find` has no substring filter, so it is NOT a live filter. It no
  longer switches the default route to saved; on the live default it is refused and points at
  `get_scene_info {query}`. `source:"saved"` still accepts it.
- **`list_entities` stays (item 12).** `get_scene_info {source:"saved"}` does not return what it does:
  the full per-class histogram (scene_info caps `classes` at 40 + `otherClasses`), up to 2000 whole
  entity records (scene_info: 500 items, four columns) and the `classnames` / `path`-free arguments an
  alias could not map. Its schema was trimmed instead (path/limit/classnames descriptions).
- `get_scene_info` live mesh bounds are still origins; the live document id is stamped as `document`.
- **Live checks owed (added):** `get_scene_info` live on a map with more than 2000 entities and on one
  with more than 4000 meshes (the 4000 page, the `null` + note path); a duplicate targetname through
  `get_entity` (live `AmbiguousTargetname`); `document` equals Hammer's open map.
- **Bytes (tools/list, compact JSON):** WP2.3 as first committed 126500 B / 91 listed (aliases hidden, no
  `game_rules`); after the review fixes 127643 B / 95 listed. That includes `game_rules` (736 B, from the
  base) and the three aliases listed again (663 B); without them 126244 B, i.e. 256 B below the first cut.
  The schema trims saved about 0.2 KB each on `get_entity`/`find_entities`, 0.25 KB on `list_entities`.

## WP3.1

1. **`look` never moves a camera, so "restore" is vacuous.** The bridge has no method that sets the
   Hammer viewport camera; `render_view` takes an explicit camera per call. `cameraRestored: true` and
   `cameraMoved: false` are constants of the design, tested by asserting that only reads and render
   calls reach the bridge, including when a render fails midway. If a camera setter appears, restore
   in a guard that also runs on error.
2. **Entity boxes are origin +- 64.** There is no extent reader for an entity (a brush entity's meshes
   are tied children). For a big brush entity pass a `{min,max}` box instead. `"map"` bounds are entity
   ORIGINS plus saved mesh extents, so a map whose geometry extends past its entities is framed short.
3. **`"map"` needs a saved document** (it calls `get_scene_info {source:"saved"}`); with only unsaved
   work it reports the file route's error, not the live document's bounds.
4. **`look` calls `render::run` directly**, bypassing the registry's capability gate; it re-checks
   `view.render` / `world.render` / `game.local` itself (`need_live`). A future change to the gate
   must keep doing so.
5. **`frames` paces with a real `tokio::time::sleep`** between renders (the images must be spaced in
   time); it stops early when the call budget has less than the pause plus 5 s left. It does not use
   `game.tick`, so the spacing is wall-clock, not game ticks.
6. **Unverified live** (acceptance run 4): that `render_world` from the framed poses shows the target
   (the mock draws a solid 8x4 PNG); that `game.local` carries `player.eye` (else origin + 64 is used);
   that `source:"editor"` poses match the engine's for the same map; `frames` against a moving
   entity; `player_eye` against ti_convoy FIXME 34 (drone camera visibility).
7. **tools/list budget:** `look` added 1837 B (80679 -> 82516 B); `MAX_TOOLS_LIST_BYTES` 81_500 ->
   83_337. The schema has no `type` on `target` (several shapes) to save a `oneOf`.

### WP3.1 review fixes

8. **All-blank engine views fail (`BlankRender`) rather than defaulting to the editor.** Chosen over a
   silent switch because the editor draws helpers/gizmos and the agent should know it left the engine.
   An engine view that is legitimately uniform (camera in a wall) fails the same way; the error text
   says both causes. A partly blank set is a success with `blankViews`.
9. **`didYouMean` pool** is built from up to three `get_scene_info {query}` calls (two longest words of
   the name, plus its first 3 letters), so it finds named entities in a 2000-entity map but can miss a
   name sharing no word and no prefix with the request.
10. Stop errors are exactly ToolBudgetExceeded, NotImplemented, BridgeBusy, PathNotMapped,
    RenderFileMissing; other per-view errors (NoGameWorld, NoViewport...) keep trying the remaining views.

## WP3.2

- **Nothing here was run against a real install or a live Hammer.** Every check is verified against the
  committed `probe.vmap`, hand-made VPKs and the mock bridge only. Owed to acceptance run 4, live:
  `report_ready` on a real compiled map (the `materials` check against the real `game/csgo` and
  `game/core` `pak01_dir.vpk`: does `materials/dev/reflectivity_30.vmat_c` etc. resolve, and does
  ti_convoy's `materials/dev/cobblestone_001.vmat` (FIXME 12) come out as missing); `saved` with a real
  dirty document; `compiled` right after a real `compile_map`; `spawn_in_mover_path` on ti_convoy (its
  lane spawn from run 3); `test_cvars` with a map running (`game.local` + `cvar.get` on the real bridge;
  the mock was not used for it); `debug_patterns` / `loose_compiled` on the ti_convoy addon (its
  scripts live in the repo `script/` tree, only the built `.vjs` reaches the addon, so the default patterns
  see the compiled side, not `config.ts`: point the check at what is really in the addon).
- **Assumption: `movedir` direction.** Source `AngleVectors` (pitch positive looks down, so pitch -90 is
  up). Not verified against a live `func_movelinear` that was run; the keyvalue is read as a vector
  or a "p y r" string, whichever the saved file stores (the real file's form is unchecked).
- **Assumption: a mover's travel is a straight line from its saved position** (movelinear: `movedistance`,
  default 100, the saved position at `startposition` / `authoredposition` of the travel; door: extent along `movedir` minus `lip`, default 8, saved position at `spawnpos`; no rotation,
  no parent). The swept volume is the union of start and end boxes, so it is exact for a box moving in
  a line. Moved parents (a mover under a moved group) and `func_tracktrain` / rotating doors are not
  checked; `detail` says which classes were present and skipped.
- **Assumption: the player hull is 32 x 32 x 72** and a spawn's origin is at the feet.
- **Assumption: mesh bounds are world space** as `get_mesh` already treats them (`MeshRecord::bounds`);
  a brush entity's tied meshes come from the `children` edges (`Document::children_of`).
- **Heuristic: asset references** are found by regex (`<path with a slash>.vmdl|vpcf|vmat|vsnd|vsndevts|vtex|vmesh|vjs`)
  in entity string keyvalues and the addon's text files. A path built at run time in a script is not seen.
- **`materials` counts a material as found in the map vpk, the addon game dir, or either game pak,** nothing
  else (no workshop item mounts, no other addon). A material that only another mounted addon provides
  reads as missing.
- **`test_cvars`: `sv_cheats` only decides the status;** other names come from `debugPatterns` entries that
  look like cvar names (lower-case, underscore) and are reported in `evidence`.
- **Merge note (WP3.1 is built in parallel):** both WPs touch `tests/server.rs` / `tests/stdio.rs` (tool
  order and counts), `tests/tools_list_budget.rs` (MAX) and `lib.rs` `build_registry`. `report_ready`
  registers last, after `new_map`; `tests/run_map.rs` expects `report_ready` as the third `next` step.
- **Bytes (tools/list, compact JSON):** 80679 -> 81950 B (+1271 B for `report_ready`); the guard's MAX went
  81_500 -> 82_771 (exactly +1271). 96 tools listed (was 95).
- **Build note:** the sl-wp32 worktree's `crates/s2sdk/upstream` could not fetch `d6b26299` (not pushed); its
  working tree was checked out from the main checkout's submodule object store. Not part of the commit.

### WP3.2 review fixes

- **`materials` now reads each resolved `.vmat_c`** and checks its `RERL` external-reference list
  (`Resource::external_refs`, new): verified against the real `game/csgo/pak01_dir.vpk`
  (`cargo test -p hammer-mcp --test report_ready -- --ignored`): `materials/dev/cobblestone_001.vmat_c`
  lists three `materials/cbble/cobblestone_ground_6/*.vtex` and none of them is in the csgo or core pak,
  which is FIXME 12's magenta. Only `.vtex` references are followed (not other external refs). A material
  that cannot be read, has no `RERL`, or any material when a game pak cannot be opened, is `skipped`
  ("resolves, textures not checked"), never a pass. At most 3000 materials are read.
- Unverified live: the `set_mesh_material {faces}` alternative on a real mesh (the tool says live
  geometry is only known for meshes written this session); `authoredposition`/`startposition` against a
  real mover.
- `walk_files` is capped at 20000 directory entries / 12 levels; a cut walk makes `debug_patterns`,
  `loose_compiled` and `materials` skipped (never pass).
- Case: disk lookups try the given case, then a case-insensitive match per path component. VPK lookups were
  already case-insensitive.
- Bytes: the description changed by +2 B ("materials (incl. textures)"): 81950 -> 81952, MAX 82771 -> 82773.

## P4

Playbooks as MCP prompts plus the `get_playbook` tool (hammer-mcp `src/playbooks.rs`).

- **GAP (cs_script): no tool builds, places or compiles a script.** `compile_asset` lists
  `vmap vmdl vpcf vsndevts vmat vtex vsnd vsmart vanim vmks`; there is no `vjs`. The compiler input is
  not a `.vjs`: ti_convoy's `sync.sh script` runs `resourcecompiler.exe -i .../maps/scripts/ti_convoy.js`
  in the tools container and gets `maps/scripts/ti_convoy.vjs_c`; its comment says a map compile does
  NOT build the script (the brief's "the map compile built the vjs_c" is not what sync.sh records).
  So the needed change is `js` (not `vjs`) in three places that must stay equal: `ASSET_EXTENSIONS` in
  `hammer-mcp/src/tools/jobs.rs`, `INPUT_EXTENSIONS` in `crates/cs2-bridge/src/ops/job.rs`, and the mock
  list in `crates/cs2-mcp-common/src/mock/jobs.rs`, plus a bridge rebuild and install, and a live run to
  see whether `job.start` with a `.js` input and the default `-game` flag behaves like sync.sh's call.
  Not done here: it needs a bridge change and a live check, and no tool for writing the TypeScript
  build output exists either (that stays the user's: building TS and placing
  `content/csgo_addons/<addon>/maps/scripts/<name>.js`). The `cs_script` playbook states both stops.
- **Assumption: playbook argument names.** `get_playbook` takes one flat set (`map`, `script`, `folder`)
  because `name` is the playbook. Prompts use the same names.
- **Assumption: `m_ChildRef` form.** The `particle_set` playbook shows `add_child {child:{m_ChildRef:"<path>.vpcf"}}`;
  the op shape is from `tests/vpcf.rs`, the resource-string form of the value is not verified in the editor.
- **Assumption: `info_particle_system` keys** `effect_name` and `start_active` come from
  research/source2-mcp-research.md, not from a live place-and-render in this WP.
- **Assumption: mover keyvalues** `movedir`, `movedistance`, `speed` of `func_movelinear` (read by
  `report_ready` `spawn_in_mover_path`; `speed` from run 4's layout).
- **Not run live.** No container was up (as the brief demands); the playbooks are checked against the
  tool schemas by `tests/playbooks.rs`, not by executing them against Hammer. The blockout path mirrors
  acceptance run 4 (25 calls, 6/6).
- **Clash risk with sl-r4fix:** both edit `hammer-mcp/src/lib.rs` (this WP: one `register` line, one
  sentence in the instructions, `.with_prompts`), `tests/instructions.rs` (`NOT_TOOLS`), and
  `tests/tools_list_budget.rs` (MAX: +686). If r4fix lands `compile_map {waitMs}` on start, the playbooks
  still work (they start without waitMs, then poll with jobId); simplify them afterwards.
## Run-4 fixes

- compile_map start+wait: the wait runs after the start returns, so the start's own work (container
  probe, orphan scan) is not inside `waitMs`; only the budget bounds the total. `waitMs` on a start
  does not set the compile deadline (`timeoutMs` still does).
- report_ready `mode`: assumed severities (defuse: no bomb site fail / no buy zone warn; hostage: both
  missing parts fail); not checked against the engine's own mode requirements. Defuse does not demand
  two sites.
- look "map": a map whose only geometry is brush/other nodes that the .vmap reader does not list as
  meshes still frames on entity origins (reported in `framedOn`). `meshBounds` is saved-source only
  in practice (live meshes have origins only, folded into the same box).
- tools/list: 83789 -> 84055 B (+266), MAX 84610 -> 84876.

### P4 review fixes

- Merged streamline/agent-ux (e6e71c8) into the branch; `compile_map {waitMs}` on start and `report_ready
  {mode}` are used by the playbooks. tools/list 84741 B after the merge (84055 + 686), 84817 B after the fixes
  (+76 B for the `get_playbook` description); MAX = 84_817.
- Playbooks never write a map path the caller did not pass; `blockout_map` still needs the user's name for
  `new_map {path}` (the text shows `maps/<name>.vmap`).
- Release verdict is `incomplete` by design (`no_metamod_load`); release, cs_script and particle_set end on
  "no fail, every warn and skipped explained", with no repeat-until-pass loop.
- Still unverified live: `movedir:"0 0 0"` as +X for `func_movelinear`, `info_particle_system` keys.

## P5
- `new_map_from_template` was not added: `new_map {template}` (hammer-mcp/src/tools/new_map.rs) already covers it.
- `find_assets` sounds are `.vsnd` files only, not soundevent names (`list_soundevents` has those). Textures are any `.vtex_c` (pak or addon); an addon `.vtex` source is rare (textures usually have png/tga sources), so `content` hits for textures are rare.
- `find_assets` paths are lowercased (addon files keep their case on disk; Source 2 paths are case-insensitive and the other tools take either).
- A material's `renders:"ok"` means: compiled, texture list (RERL) read, every `.vtex` it names found in the addon, the loose game dirs or a pak. It does not look at shaders, other external refs, or whether a texture itself is valid.
- `facing`: attachment offsets are in the bone's frame. They count only for a single-influence attachment on a bone whose chain to the root has identity rest rotation and unit scale (read from `m_modelSkeleton`); front/rear are then compared only on one shared bone. A rotated chain or attachments on different bones are `unknown`; composing the bone transforms to get model-space positions is not done (the review fix chose no vote over a guess). car_a and utility_truck carry no attachments, so FIXME 21's -x is still only knowable by `model_render` from two sides.
- tools/list budget: P5 adds +1156 B (find_assets schema + description, get_model_info facing text); after merging agent-ux (get_playbook) MAX = measured 85973.
- report_ready's materials check now calls `Sources::check_textures` (moved with `Paths`, `Sources`, `open_vpk`, `find_ci`, `norm` into `tools/asset_resolve.rs`); behaviour unchanged (its tests pass).
- Review fixes: truncated/malformed RERL = not checked (find_assets `unknown`, report_ready skipped); model finder reuses the cached catalog + `addon_scan`; the file-walk bound now counts every visited file (also changes the model catalog's addon walk: a tree of >200000 files of any kind stops the walk and says so); notes name the kind (`addon material walk`).

### P5 live smoke (2026-10-27, orchestrator)

- Live on accept_p4: `find_assets` flags FIXME 12's `materials/dev/cobblestone_001.vmat` as
  `missing_textures` (the three `cobblestone_ground_6` `.vtex`); `compile_map {mode:"novis", waitMs}`
  finished in one call (15 compiled, 0 failed); `report_ready {mode:"none"}` pass; `look {target:"map"}`
  framed on mesh extents; `get_model_info` car_a facing `unknown` (no attachments, as expected).
- Fixed after the smoke: a resource with no `RERL` block was `renders:"unknown"` (111 of the first 200
  `materials/dev/` hits, all texture-less shaders). It now references nothing (`ok`); only a present but
  malformed list is unknown. Assumption: the compiler omits RERL when the list is empty (consistent with
  which materials lack it; not read in the compiler).

## cs_script

Branch streamline/cs-script (bridge m3.17, hammer-mcp `cs_script` tool).

- VERIFIED LIVE 2026-10-28 (accept_p4, bridge m3.17): the engine loads only `maps/scripts/<name>.vjs_c`. A map
  loaded before it exists prints `Failed loading resource "maps/scripts/main.vjs_c"` and `[cs_script] Failed to
  load cs_script "maps/scripts/main.vjs"`, runs without the script, then the tools engine compiles the `.vjs_c` on
  demand (only the next load has it). Fixed: `build` now names `vjs_cMissing` and points at `compile` BEFORE
  `run_map`; the playbook compiles in step 3. (The old text said the `.vjs_c` was only for shipping: wrong.)
- VERIFIED LIVE: hot reload. A changed `.js` while the map runs prints `[AssetSystem] Reloading Resource [1/1]:
  maps/scripts/<name>.vjs`, an on-demand recompile (`file CRC changed`), then the script's Msg as
  `[cs_script] [<name>] loaded`; `build` returned `reloaded:true` with that line (1.3 s). The engine never printed
  `Loading script`; that marker is gone. The `.vjs_c` path `game/csgo_addons/<addon>/maps/scripts/<name>.vjs_c`
  is right; `compile` through the bridge job made it for a new script.
- GAP: no `remove` action. A script can be added but not deleted through the MCP (I deleted `src/broken` by hand
  in accept_p4 and the next `compile` failed `ScriptNotBuilt broken` until its rollup target line was removed by
  hand too). accept_p4 keeps a test script `door` and `main`.
- LIMIT: `build` watches the console 4 s for every changed script, even one no point_script uses (`door`); the
  reply then says `unknown`. Could skip the watch for scripts `status` finds unreferenced.
- HACK (harness, not repo): `target/orch/c.sh` in the main checkout, a one-line wrapper over `mcp_session.py call`.
- HACK, types comparison: the npm package ships tsc's declaration output, not Valve's text, so a text diff can
  never match; the check runs a TypeScript-API script (`api-fingerprint.mjs`) under node. It needs the project's
  `typescript` 5.x (TypeScript 7 has no JS API); an exported type renamed without an API change counts as drift
  (the override is harmless then). Offline `update` still compares what is installed.
- HACK, override mechanism: ambient `declare module "cs_script/point_script"` from `@s2ze/types/autoimport` beats
  any `paths` mapping, so the override drops the npm autoimport AND extended types (`types: []`) and includes the
  game's file. Script tsconfigs the tool cannot parse (hand-edited with comments) are skipped and listed as
  `unparsableTsconfigs`.
- LIMIT: no podman fallback for `.js` (compile_asset has none either). An installed bridge older than m3.17 refuses
  `.js` inputs; the error says to reinstall the bridge.
- LIMIT: `npm install` that outlives the call keeps running as a server-side job; if the server stops, it dies with
  it and the next init/update/build runs it again (npm is idempotent). Only one process per project dir.
- LIMIT: the OUTPUT_DIR in rollup.config.js is an absolute host path (brief decision); moving the addon content
  needs the line edited (or a fresh init).
- NOTE: `typescript` is pinned `^5.9` by the template; `npm view typescript` says latest is 7.0.2, which has no
  `typescript` JS API for the fingerprint and may change rollup's typescript plugin. `update` does not touch it.

## Live test 2026-10-28

- FIXED, every tool: the registry dropped unknown argument names silently; `find_entities {"classname":...}`
  returned all 20 entities unfiltered (the key is `class`). Now `InvalidParams` with `unknownArguments` and
  `didYouMean` (`check_argument_names` in cs2-mcp-common `tools.rs`; keys of top-level oneOf/anyOf/allOf
  branches count; open schemas accept anything). Seen live after the fix.
