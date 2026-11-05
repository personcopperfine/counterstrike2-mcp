# Protocol changes in m3.12: the last blocked rows (jobs, mesh read-back, selection)

Bridge build `m3.12` is m3.11 plus the unblock-rows project (branch `bridge/unblock-rows`, design
`target/orch/unblock-rows/DESIGN.md`, RE notes `target/orch/unblock-rows/re-report.md`). Five
rows that answered `-32011 NotImplemented` are methods now: `job.status`, `job.cancel` (plus the
new `job.start`), `mesh.get`, `selection.get`, `selection.set`. `map.run` stays blocked with a new
`blockedOn` text.

This file is the wire contract a client or a test can rely on. The 16 new Hammer pins
(`sigs/signatures/hammer.json`, hammer.dll 2000933) were read in the binary by the RE lane and an
independent review; the 15 the new rows call are `verified` since the live acceptance run
(section 7). `HFN_MESH_EDGE_POSITIONS` stays `proposed`: it is never called (see section 3).

## 1. The registry overlay (`RUST_OVERRIDES`)

`registry::METHODS` stays the frozen C table, row for row (the C differential in
`tests/registry.rs` still holds 84 rows / 69 implemented). A new table `RUST_OVERRIDES` replaces
a C row for this build: `find`, `gate`/`gate_with`, `dispatch::decide`/`is_ported` and the
Rust advertisement read the override first (`registry::effective`). The C-identical paths (the
IPC/EXEC gate differential) use `gate_c_with` on the raw table. Rules, held by
`tests/overrides.rs`: every override names a C row, one per name, never also a `RUST_METHODS`
name, `needs` equal to the C row's unless listed in `NEEDS_MAY_DIFFER` (empty), no duplicate
advertisement. `bridge/bridge_build.py` `methods()` returns the effective table, `c_methods()` the
raw one, and refuses a missing or empty table.

| Row | m3.11 | m3.12 |
|---|---|---|
| `job.status`, `job.cancel` | `-32011` (protocol §7 reserved) | implemented, needs 0, lane-answered |
| `job.start` | (none) | new `RUST_METHODS` row, needs 0, lane-answered |
| `mesh.get` | `-32011` ("reads come from the saved .vmap") | implemented, `NEED_HAMMER`, queued |
| `selection.get`, `selection.set` | `-32011` ("not isolated") | implemented, `NEED_HAMMER`, queued |
| `map.run` | `-32011` "hammer-save-compile.md §B" | `-32011` "superseded by hammer-mcp run_map (VToolMessage command channel)" |

## 2. Jobs: `job.start` / `job.status` / `job.cancel` (kind `resourcecompiler`)

A bridge job runs `resourcecompiler.exe` for the calling client: the client starts, polls and
cancels; where and how the compiler runs is the bridge's business. The three rows are answered on
the IPC lane (`wiring::BridgeState::route_with_params`, like `metamod.plugins`), never queued: they
answer while Hammer sits in a modal or a map load freezes the frame loop.

**`job.start {kind:"resourcecompiler", input, game?, flags?, timeoutMs?, clientRef?}`**
-> `{jobId, state:"running", pid, log:{windows, relative}, command:[argv...], startedAtMs}`

- `input`: exactly `csgo_addons/<addon>/.../file.<ext>`, `/`-separated, segments of
  `[A-Za-z0-9_.-]` (no `.`/`..`/empty), at most 260 bytes, extension one of `vmap vmdl vpcf
  vsndevts vmat vtex vsnd vsmart vanim vmks`, and the file must exist under the content root.
- `game:true` adds `-game <gamedir>\csgo` (the model/particle/soundevents recipe); false is the
  map recipe (no `-game`).
- `flags`: tokens from a fixed allowlist, each at most once: `-nav -fshallow -quiet
  -unbufferedio -noassert -nop4 -f`, and `-maxtextureres N` (1..16384), `-threads N` (1..64) as
  two tokens. Never a free command line. `-i <absolute input>` is always last.
- `timeoutMs` 1000..3600000, default 1500000.
- `clientRef`: the caller's own token (1..=64 bytes of `[A-Za-z0-9_.-]`), echoed by `job.status`,
  so a caller whose start reply was lost finds exactly its job (hammer-mcp adopts by it only).
- `jobId` is opaque, `j<N>-<nonce>` (the nonce is the table's creation time, base 36): unique
  across bridge loads. Pass it back verbatim.
- The log is `<app>\cs2mcp_jobs\<jobId>.log` (`relative` = `cs2mcp_jobs/<jobId>.log` from the app
  dir; on the container host `workshop/run/app/<relative>`). It does not follow
  `CS2_BRIDGE_LOG`. An evicted job's log is deleted.
- Errors: `-32602 {"reason":...}` for every parameter problem (echoed tokens cut to 64 chars);
  `-32017 PrerequisiteFailed {"blockedOn":...}` with `job: resourcecompiler is Windows-only`
  (Linux), `job: <id> running`, `job: another job is starting`, `job: resourcecompiler.exe not
  found at <path>`, `job: bridge is shutting down`, `job: cannot locate the game directory from
  ...`, `job: the host executable path is unavailable`, `job: AssignProcessToJobObject failed:
  <os error>`; `-32603 {"reason":"spawn failed: ..."}`.

**The process.** `CreateProcessW` of `<gamedir>\bin\win64\resourcecompiler.exe` (located from
`GetModuleFileNameW(NULL)`), cwd that directory, created suspended, assigned to a Win32 job object
with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resumed. stdout+stderr go to the log; the child
inherits only the log and NUL handles (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`). Every child
(`vrad3.exe` included) is in the job: cancel, timeout and the OK-line teardown are
`TerminateJobObject`, and a bridge unload or a `cs2.exe` exit cannot leave a compiler running. At
most one job runs; 16 finished jobs are kept (oldest evicted).

**The watcher.** Reads the log incrementally and lossily (non-UTF-8 never blanks it), to EOF on
exit. ` OK: N compiled, F failed, S skipped` (leading space, CRLF) ends the tree at once and the
state is `failed` if F > 0, else `ok`. A `FATAL ERROR` line without an OK line within 10 s ends it
`failed`. A process exit without the OK line is `failed` with `exitCode`. The deadline is
`timeout`. A cancel that wins stays `cancelled`.

**`job.status {jobId}`** -> `{jobId, kind, state, input, clientRef, pid, elapsedMs, failLines,
fatal, logTail, log, startedAtMs, exitCode?, summary?:{compiled, failed, skipped, seconds}}`,
`state` one of `running ok failed cancelled timeout`; `logTail` is the last 40 lines. `{}` ->
`{jobs:[{jobId, kind, state, input, clientRef, elapsedMs, startedAtMs, log}]}`. Unknown id ->
`-32602 {"reason":"unknown jobId"}`.

**`job.cancel {jobId}`** -> `{cancelled:true, state:"cancelled"}`; a finished job ->
`{cancelled:false, state:<final>}` (not an error); unknown or missing id -> `-32602`.

**`control.status`** `jobs` (always `[]` in the C) now lists the table: `[{id, kind, state}]`.
cs2-mcp-common's frame-loop grace treats `kind:"resourcecompiler"` (and a kind-less entry) as
non-freezing: a compile never excuses a real Hammer modal as "engine busy".

**Teardown.** `wiring::stop_runtime` (tools Shutdown/PreShutdown, an mm-only Metamod Unload)
terminates running jobs and joins the watchers within 1.5 s; an unjoined watcher refuses the
unload, so no job thread outlives the image.

## 3. `mesh.get` (live mesh read-back)

**`mesh.get {nodeID, expectDoc?}`** ->
`{nodeID, class:"mesh", origin, angles, scales, space:"local", vertices:[[x,y,z]...],
faces:[[v...]...], faceMaterials:[i...], materials:[name...], vertexCount, faceCount,
worldSpace, parentChain:[class...], doc:{id, topSerial, undoDepth}}`

- `parentChain` is the node's ancestors' classes up to `world` (a guarded `CMapNode::m_pParent`
  walk, at most 64); `worldSpace` is true only when every ancestor is a plain group/entity/world
  (`tie_ancestors_ok`'s rule). An unreadable parent is `worldSpace:false`, never assumed world;
  hammer-mcp's rebuild operators refuse such a mesh.

- Read-only: no undo group, no dirty bit, the fingerprint is unchanged. Queued (main thread).
- `vertices` are node-local, in `CollectHandles(poly vertices)` order, read with ONE batch call
  of Hammer's checked vertex-position reader (`HFN_MESH_VERTEX_POSITIONS`, identity matrix; a
  missing position stream is `-32603 "mesh has no position stream"`, a bad handle reads (0,0,0)).
  World = `origin + R(angles) * (scales * local)`, the same transform the saved-file reader
  applies.
- `faces` in `CollectHandles(poly faces)` order (the saved .vmap order); each a loop of vertex
  indices from the half-edge walk (first edge, next, edge vertex; all handle-checked), the
  CreateFace input order with a possibly rotated start, counter-clockwise from the visible side.
- `faceMaterials` index `materials` (deduplicated, first-use order); `-1` = no material.
- Refusals: unknown `nodeID` -> `-32022 NodeNotFound` (the node ops' convention); not a mesh -> `-32602`; `expectDoc` mismatch -> `-32004`; a mesh
  whose reply could exceed `maxLineBytes` (estimated before the walk at 24 bytes per float, and
  more than 1,048,576 corners) -> `-32602 {"reason":"mesh too large for one
  reply", vertexCount, faceCount, maxLineBytes}` before any walk; a walk that does not close ->
  `-32603`; a failed origin/angles/scales read -> `-32603` (never a zero default).
- Not called: `HFN_MESH_EDGE_POSITIONS` (reads address 0 for a half-edge without an opposite;
  kept as a pin only).

hammer-mcp: `get_mesh` without `path`, the rebuild operators and per-face `set_mesh_material`
read the live geometry (`source`/`geometrySource:"live"`, no dirty refusal); the session
`MeshCache` and its sticky undo-state rule are gone. Without `mesh.get` the file route with its
dirty gate runs as before.

## 4. `selection.get` / `selection.set` (nodes and faces)

The document selection is `CMapDoc::m_pSelection` (header field). Hammer keeps 8 selection sets
(one per mode); the mode is read by comparing `GET_SET(sel, k)` with `CURRENT_SET(sel)` for
k = 0..7: `groups objects meshes vertices edges faces mixed tile`, `null` when none matches.

**`selection.get {}`** -> `{mode, count, nodeIDs:[...], faces:[{nodeID, faces:[i...]}...],
unmapped}`. In modes 0..2 `nodeIDs` are the selected nodes; in face mode `faces` lists per mesh
the selected face indices (positions in `mesh.get`'s face list, ascending) and `nodeIDs` those
meshes; in vertex/edge/mixed/tile mode only `count`. `unmapped` counts selected items the bridge
could not name (outside the tree walk's caps, no id, a mesh whose faces could not be collected).
An unknown mode calls nothing further and answers `mode:null, count:0`.

**`selection.set {op, nodeIDs? | faces?, expectDoc?}`**, `op` one of `replace add remove clear`
-> `{requested, applied:<the get object re-read> | null, appliedError?, modeChanged,
undoGroupOpened:false, doc}`.

- Everything is validated before the first Hammer call: the gate (`-32003`/`-32018`/`-32004`),
  exactly one of `nodeIDs`/`faces` (neither for `clear`), at most 4096 ids / 65536 face indices,
  every id in the live tree (`-32602 {"reason":"unknown nodeIDs","unknown":[...],
  "unknownTotal":N}`), face targets are meshes (`{"reason":"not a mesh","nodeID"}`), every index
  in range (`{"reason":"face index out of range","nodeID","faceCount","badIndices"}`), and a
  known current mode (`-32603 {"reason":"selection mode unknown"}` otherwise — Hammer's own
  functions dereference a null/garbage set there).
- Nodes: a mode outside 0..2 switches to `objects` (`SET_MODE(sel, 1, convert=0)`). Faces: a
  mode other than 5 switches to `faces`. With `convert=0` Hammer converts nothing, so after a
  switch the bridge clears the target set first: `add`/`remove` after a switch are relative to
  an empty selection (`modeChanged:true`).
- Face `replace` = clear (`SELECT_NODES(empty, op 1)` in face mode), then `SELECT_FACES` op 2 per
  mesh; `add` = op 2, `remove` = op 3. `clear` = `SELECT_NODES(empty, op 1)` in the current mode
  (it clears only that mode's set).
- Hammer may filter (locked/hidden nodes; a mesh that ignores face selection): `applied` can be
  smaller than `requested`; that is not an error. A re-read failure after the change answers
  success with `applied:null` and `appliedError`.
- No undo record and no dirty bit (Hammer's selection is not undoable); the UI is notified
  through Hammer's own listeners. Face selection writes per-face selected flags into the mesh.

hammer-mcp: `get_selection` (ReadOnly) and `set_selection` (Mutating: write gate + expectDoc, no
unsaved-edit count), capability-gated.

## 5. hammer-mcp job tools (consumers)

`compile_map`, `model_compile` (route resourcecompiler), `particle_compile` and
`compile_soundevents` run as bridge jobs when `job.start` is advertised (`route`/`compiler:
"bridge"`), keeping their host-side gates and verdicts (vpk/`_c` mtime, `sourceChanged`, dirty
gate, in-game refusal); they fall back to the podman exec route only when the bridge cannot run
the compiler here (Windows-only, exe not found, job-object assignment failed). New tools:
`compile_asset {path, game?, flags?, timeoutMs?}`, `job_status {jobId?}`, `job_cancel {jobId}`;
`compile_map {jobId, cancel:true}` cancels on either route. Every start carries a `clientRef`; a
lost start reply is adopted by it; a start racing the server's shutdown is cancelled.

## 6. Tests

- Host: `crates/cs2-bridge/tests/{overrides,job}.rs`, `ops::mesh` / `ops::selection` unit tests,
  `hammer-mcp/tests/{bridge_jobs,compile_map,mesh_live,mesh_ops,selection_live}.rs`,
  cs2-mcp-common's mock `job.*`/selection/`mesh.get`.
- Live: `bridge/test_unblock_rows.py` (Wine `-tools`, `maps/probe.vmap`), `--static` offline.
  Results of the m3.12 acceptance run: see section 7.

## 7. Live acceptance (2026-10-20, Wine `-tools`, hammer.dll 2000933, `maps/probe.vmap`)

| Run | Result |
|---|---|
| `test_unblock_rows.py --static` | 31/31 |
| `--only version` | 8/8 (build `m3.12`, rows advertised, `map.run` superseded) |
| `--only mesh` | 17/17 (box read back in local space; read-only: undoDepth/topSerial/dirty unchanged; follows `mesh.setGeometry` and its undo; unknown nodeID `-32022`) |
| `--only selection` | 28/28 (nodes and faces, add/remove/replace/clear, mode switches, refusals change nothing, undoDepth/topSerial/dirty unchanged) |
| `--only jobs` | 11/11 (soundevents job ok; map job; second start `-32017`; `control.status` kind; cancel -> `cancelled` with no `resourcecompiler`/`vrad3` process left in the container; cancel of a finished job `cancelled:false`) |
| `--only jobs --full-map` | 9/9; the map job ran `vrad3.exe` inside the job and ended on ` OK: 19 compiled, 0 failed, 1 skipped, 1m:01s` |
| real `hammer-mcp` binary over stdio | 17/17: the five new tools listed; `create_block` -> `get_mesh` (live, 8 vertices / 6 faces) -> `set_selection` faces/nodes/clear -> `get_selection`; `compile_asset` (soundevents, `-f`) -> `job_status` ok -> `job_cancel` (finished: not an error) -> `undo_last` |

Not run live: a mesh under a transformed group/entity (`worldSpace`/`parentChain` on a real
nested node), the too-large refusal on a real >13k-vertex mesh, the Linux `job.*` refusal (host
tests only).
