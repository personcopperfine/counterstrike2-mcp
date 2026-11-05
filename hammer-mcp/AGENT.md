# hammer-mcp — agent guide

Rust **stdio MCP server** that drives the CS2 **Windows workshop tools** (Hammer first; ModelDoc,
particle editor `pet`, asset editor/browser next; SFM last) through the in-process bridge DLL,
with offscreen renders + OCR so a model can *see* the result.
This file is self-contained: it repeats the conventions shared with `../client-mcp/` on purpose.
The **code** those conventions describe is shared through the `cs2-mcp-common` crate.

Status (2026-10-14; the trigger block was accepted live end to end on 2026-10-16):
`session_status` works (workflow 00). **Triggers are done** (branch `hammer/triggers`, bridge
m2.19 `entity.tieMeshes`/`entity.untie`/`entity.get` children, verified live — the acceptance map
compiled and the triggers fired at runtime: see `## Triggers` below). **Entities are done** (branch
`hammer/entities`, bridge m2.15, verified live): `list_entities` reads the SAVED `.vmap` (`src/vmap`, a Rust
port of the binary-DMX reader); `get_entity`/`find_entities` read the live document or the saved file
(`source:"live"|"saved"`, WP2.3: see `## WP2.3` below; `get_entity_live`/`find_entities_live` are
deprecated aliases, still listed for one release); `get_scene_info` is the one-call overview; `create_entities`, `set_entity_keyvalues`, `delete_nodes`, `undo_last`,
`save_map` edit it; `reload_map` reloads the live map from disk over `map.reload` (refuses
an untitled document, and dirty/dirty unknown unless `force`, resets the unsaved counter only on a
confirmed `reloaded:true`; the bridge arm carries the same untitled gate and checks the
`doc.active` `expectDoc` as of m2.16, so a document switch is `DocumentChanged`); `open_map` opens another `.vmap`
over the m2.16 bridge `map.open` (the canonical path shape, the host file, the file's own binary-v9
DMX header — this server parses it with `src/vmap` first: a file that fails to load would pop
Hammer's modal "Error opening file" on the tools main thread — and an already-open session are gated
before `OpenFile`; a file that is already open is `existing:true` with a note, never a second
session);
`list_entity_classes`/`get_entity_class`
describe classes from the game's FGD files
(`src/fgd`), which also validate keyvalues before any edit. Keyvalues are always sent as strings
(Hammer stores every FGD keyvalue as a DME string). `undo_last` pops a keyvalue group without
restoring values: `set_entity_keyvalues` returns `previous` for a manual revert. **Mesh file
reads are done** (WP-A): `list_meshes`/`get_mesh` read the CMapMesh nodes of the SAVED
`.vmap` (`src/vmap/mesh`, a port of the POC's `vmap_reader.py` `_mesh_from_element`: world
vertices, edge-walk faces, per-face materials). **Mesh writes are done** (WP-B, bridge m2.5 methods):
`src/geometry.rs` is the full Rust port of the POC's `geometry.py` (validate, materials, normals,
centroids, bounds, winding, box/wedge/cylinder, rotate/transform/mirror, extrude, move vertices,
delete/flip faces, with its unit tests) and `src/tools/mesh_live.rs` drives `mesh.create`
(`create_mesh`, `create_block`), `mesh.setGeometry` (`set_mesh_geometry`, the four rebuild
operators `transform_mesh`/`move_mesh_vertices`/`extrude_mesh_face`/`delete_mesh_faces`, and the
per-face `set_mesh_material`) and `mesh.setMaterial` (`set_mesh_material` for every face) — all
live mutators with `expectDoc`, one undo group each (the bridge's own `MCP: …` labels), no
auto-save. The rebuild operators (WP-C), the per-face `set_mesh_material` and `get_mesh` without `path` read
the mesh's geometry **live from Hammer** through the bridge's `mesh.get` (m3.12,
`src/tools/mesh_read.rs`): the reply is node-LOCAL geometry plus the node's own
`origin`/`angles`/`scales`, converted to WORLD with the very transform the saved-file extraction
applies (`vmap::mesh::local_to_world`), so every route sees the same `MeshRecord`. The result
reports `geometrySource:"live"` (`get_mesh`: `source:"live"`, `staleRead:false`), and there is no
dirty gate on this route: it is a read of the document itself, so an undo, a redo, a Hammer UI edit
or another client between two calls is simply seen by the next read. Each call sends the active
document's fingerprint as `expectDoc` (a tab switch in between is `-32004`, never another
document's mesh). Only a bridge without `mesh.get` (older than m3.12, or `hammer_mesh_get(false)` on
the mock) takes the file route: the SAVED `.vmap` (`src/vmap` `mesh_by_id`, `geometrySource:"file"`)
while the live document is clean — a dirty, dirty-unknown or MCP-edited document is refused with
`-32016 UnsavedChanges` and a hint to `save_map` first, a scaled file mesh and an unproven parent
chain are refused — and `get_mesh` with `path` always reads the SAVED file (`source:"file"`,
`staleRead`). The operators return the rebuilt counts (`vertexCount`/`faceCount`), so a client need
not re-read. A server-side geometry cache (`MeshCache`, with a sticky whole-cache rule against the
document's undo state) stood in for this read until m3.12 and is gone, together with its
invalidation rules (undo, reload, open, maybe-executed failures) and `geometrySource:"bridge-cache"`.

Review-5 L1: tool calls **run concurrently** (rmcp spawns one task per request), so every counted
write holds the server-wide write gate (`state.rs` `HammerState::write_gate`, one async mutex) for
its whole bridge sequence — the pre-write `doc.active` read through the write and its reply
(`create_mesh`/`create_block`/`set_mesh_geometry`/`set_mesh_material` + the four rebuild operators,
`create_entities`, `set_entity_keyvalues`, `delete_nodes`, `undo_last`, `reload_map`, `open_map`,
`save_map`). One lock per tool call, acquired only at the top of those handlers — never in a helper
(`send_geometry`, `current_mesh`, `require_document`, `read_active_document`,
`set_all_faces_material` run inside an already-locked section and must not lock again), and never
across anything that waits on the user. `concurrent_mesh_writes_and_an_undo_...` in
`tests/mesh_live.rs` pins the invariant that the live read of a mesh is what the document holds
after any interleaving. Since m2.17 a write's post-state is the `doc` member of its own reply (no
second `doc.active`; `live_common::post_write_document`).
The operators'
write carries the fingerprint of the document the source resolution already read (review-2 L2: no
second `doc.active` between the gate and the request; a mid-call tab switch is refused pre-write
by the bridge's own `expectDoc` gate). The bridge
applies `faceMaterials` only when Hammer's face count equals the one sent, so a mismatch
(`faceMaterialsApplied:false`, a note naming the counts) is reported; later per-face edits read
the faces Hammer really holds. A node's object-properties `scales` needs no refusal on either
route (review-2 M1 is retired in m3.12): the file's vertices are already world-scaled, `mesh.get`'s
local vertices are scaled to world by the same transform, and the bridge's rebuild
(`geo_reset_mesh`, m2.18) puts the node's scales, angles and pivot back to identity before it builds
from the world points. What is still refused is a mesh whose ancestor chain is not proven
(`reason:"unsupported parent"` / `"unrooted mesh"`, review-3 NEW-5), on the file route from the saved
tree and on the live route from `mesh.get`'s `parentChain` (`worldSpace:false`): `origin`/`angles`
are the node's own, and only `CMapGroup`/`CMapEntity` below `CMapWorld` are proven to store their
children in world space (all 4059 meshes of the installed content tree are, `vmap_mesh`'s
`#[ignore]`d corpus test), so a deformer, an instance, a prefab or a mesh outside the world tree is
refused (`get_mesh` still answers, with `worldSpace:false` and a note). One `mesh.get` reply is one
line (`maxLineBytes`, 1 MiB): a mesh that cannot fit (tens of thousands of vertices, estimated
pessimistically before the walk) is the bridge's `-32602 {reason:"mesh too large for one reply",
vertexCount, faceCount, maxLineBytes}`, which this server maps to `MeshTooLarge`; the rebuild
operators and `get_mesh` then take the SAVED file while the document is clean (a note says so,
`geometrySource:"file"`) and are refused with the counts and the `save_map` hint otherwise.
refused. `set_mesh_material` on a mesh whose geometry was emptied is
`MeshEmpty` (retry-safe: every `mesh.setMaterial` `-32603` exit precedes the undo group).
**Renders are done** (WP-D + review-2 lows): `render_view`/`render_world` over
`view.render`/`world.render` (`src/tools/render.rs`). A caller-given `name` is sent; without one
the tool sends no `name` and the bridge names the file (`render_<tick>_<seq>`, its global
sequence — two concurrent servers cannot overwrite each other's PNG, review-2 L3). A render that
succeeded but whose Windows path does not map to a host file fails hard only when an inline image
was requested; `inline:false` answers `hostPath: null` plus a note (and skips the local pixel
decode, review-2 L4). `render_world` with no map loaded still renders the engine's empty scene
world (a black image plus the blank note); `-32021 NoGameWorld` is only the engine-with-no-scene-
world case (review-2 L6). **Run is done** (WP-R): `run_map` loads the compiled `.vpk` in the tools
session's engine over the VToolMessage `command` channel (`src/tools/run_map.rs`, no bridge method
at all) and watches `game/csgo/console.log` for the load signal.

`run_map` (WP-R) is the proven headless load route, not the POC's `map.run`: the bridge's
`map.run` is `-32011` (and `console.exec` is `-32011` on a bridge older than m3.13, whose gate was
`-testscript` only), while the engine's **command channel** answers any
session — `vtmsend.exe` posts a kv3 `command` payload and the engine's `console` runs the line
(`runtime.sh` `wait_channel` and the bridge load itself use it; the shared helpers are
`cs2_mcp_common::runtime::docker::{vtm_command_payload, vtmsend_program_argv}`). It is **not** the
console tool (`console_exec`, "Console" below): the command templates are fixed (each optional `game_type N`/`game_mode M` as its own
post — the proven recipe, and whether the channel splits a line on `;` is unverified [review-1 L4]
— then `map <name> nomapvalidation=1`), the map name is validated against `[A-Za-z0-9_/-]` (no space,
quote, `;` or control byte — the name is a console argument), and `maps` is never built (that
console command crashes the engine, 2026-10-07). The map comes from `path` (the same canonical
`maps/…vmap` gate `open_map` uses) or from the active document; the **compiled** `.vpk` must
already exist — it is looked up under `game/csgo_addons/<addon>/` in two spellings
(`maps/<x>.vpk`, the compiler's own content→game shape, whose engine name keeps the `sub/x`
spelling, and the flattened `maps/<stem>.vpk` of the `compile_map.sh` recipe, whose engine name is
the bare stem; a sub-directory map accepts the flattened one only when it is the only candidate,
**newer** than its `.vmap`, AND no top-level `maps/<stem>.vmap` SOURCE exists — a fresh top-level
compile of the same stem is exactly the unrelated vpk a newer-than-vmap check alone would accept
(review-1 M8, completed in review-2), reported as `vpk.form` +
`data.flattenedRejected`) — and a missing one is
`VpkNotFound` before anything is sent, with the candidates and a `compile_map` hint in `data`. A
`.vpk` older than its `.vmap` is `source.staleCompile` + a note (it still runs). A dirty or
dirty-unknown document is **not** a refusal: a load does not touch the document, so it is only a
note saying the engine loads the `.vpk`, which does not contain the unsaved edits.
**A map the engine already has mounted is unmounted first** (`remount` in the result, review-1 H1):
the engine keeps serving the copy it loaded when the `.vpk` is recompiled under it, so the tool
posts `disconnect` (the proven cure, HANDOFF "Playtest" item 1 / playtest 2's `step4_reload.py`) and
waits for the engine's idle level (`Spawn Server: <empty>`, 120 s bounded by the budget) before the
map command; `game.local` (m2.9, when exposed — `inGame:false`/`map:"<empty>"` at the idle level,
and the live `-32024 NoLocalPlayer` answer — what the engine says when it is in NO map at all,
right after a disconnect — resolves to `inGame:false`, never "unknown")
says whether a map is up, and this server's own last successful load is the fallback when the bridge
has no `game.local`. A `disconnect` whose idle-level line never appears means the engine may still be
loading that level, and the map command is **not** sent (`state:"failed"`, `remount.failed:true`) —
a console command during a load is the documented crash trigger. **All of this is decided inside
the vtm_gate with FRESH inputs** (review-2 N2): a `game.local`
read taken before the gate wait could be STALE by the time it is used (a concurrent call of
this server may have posted a whole load in between), so the gate's holder re-reads `game.local`
and re-opens its console watch (a FRESH offset right before the `disconnect`) — an idle line
from ANOTHER call's disconnect can never be matched as this one's. The record of a failed
remount (or a call whose `disconnect` landed but nothing after it) is an IDLE-level record
(`LastLoad.idle`): its settle target is the engine's idle line (or `game.local` inGame=false),
never a `Loading map "<name>"` line that will never come (review-2 N3).
The verdict is the engine's own console line: the tool records the console.log offset at send time
(`ConsoleWatch`, pt_lib.py's `console_size`/`console_since`; a replaced console.log restarts at 0 by
inode, review-1 L5) and polls it (250 ms, never a blind sleep) for `Loading map "<name>"` and then
`OnSwitchLoopModeFinished( game : success )`, or a failure line (`invalid map name`, `Failed to load
map`, …) — the live defuse playtest saw the success line 9 s after the command. The success — and
every failure but the engine's own map-name validator — is only accepted **after** that
`Loading map "<name>"` line: a leftover success from an earlier load cannot answer for this one
(review-1 M5; a `game.local` naming another map downgrades the result to `state:"unconfirmed"` —
note that whether the engine reports a sub-directory map as `sub/x` or by its stem is still
unverified live, so a genuine load of `maps/sub/x.vmap` could read as `unconfirmed` if the engine
answers with a different spelling; the orchestrator's live pass should settle it).
It returns a **result** for every outcome (`state: ok|failed|timeout|unknown|unconfirmed`, `ok`
false for all but the last of those, with the console tail it saw; a timeout is not an error because
the command was already delivered and the load may still be coming), plus `{inGame, map}` from
`game.local` when the bridge exposes it (m2.9; optional, never part of the verdict on its own, and
read with what is **left** of the tool budget after the watch [review-1 L6]). `timeoutMs` is at most
600 s, always within the remaining tool budget: the **default** is `min(60 s, what the budget
leaves)` and a default cut that way is SILENT (the result's `timeoutMs` reports what was really
watched), while only an **explicit** `timeoutMs` the budget had to cut sets `budgetClamped` and the
"cut short by the tool budget" note [review C]; the `disconnect` wait's own budget note follows the
same rule (`disconnect_wait`). The `game.local` lookup connects the bridge first: the client connects
lazily, so `has` before the first connect would report the method absent — which it is not (an
`unload` can be the first bridge call the server ever makes).
/**`unload: true`** is the same route's disconnect half, and the primitive `compile_map`'s
map-in-game refusal points at: `path`/`gameType`/`gameMode` are refused locally (it sends **only**
`disconnect`), and the whole decision — the fresh `game.local` read, the previous-record settle
and the fresh console offset before the `disconnect` — happens under the same `vtm_gate` (N2).
The engine's idle level is waited on (`timeoutMs` bounds the wait, `disconnect_wait` the recipe's
120 s). `state`:
`ok` = the idle level was seen (this server's own load memory is then cleared, so the next `run_map`
does not disconnect for a map that is not up); `failed` = the post did not come back
(`delivered:false`, never repeated); `timeout` = posted, no idle line (including a session without
`-condebug`); and a **no-op** (`noop:true`, `state:"ok"`, `disconnected:false`, nothing sent) when
`game.local` says the engine is not in a map — where an UNRESOLVED record is settled FIRST and
KEPT unless it settled (review-2 N9: `game.local` can answer inGame=false while a load is in a
non-frozen phase, so the no-op must not silently drop it). Unload holds no write gate and does
not touch the document either.
Three more review-1 fixes shape the route: **one load at a time** — a call whose engine gets no
verdict keeps a [`LastLoad`] record, and a later call first settles it in the console (up to 20 s,
or `game.local` saying a map is up) and refuses `EngineBusy` when it is still unresolved, because a
`map` posted into a loading engine is that same crash trigger [M6 — and review-2 N3: the record
is written only once a `map` post (or a `disconnect` that can be followed by nothing) has gone
out; a post that FAILED before delivery (`OpenMutex failed`, no container) leaves NO record and
releases the external grace, so a failed call can no longer lock out `run_map` AND the unload
recovery primitive for the whole record window]; **a post is never repeated once
it may have been delivered** — only the `crun` *spawn* failure (which cannot have reached the
container) is retried, a `podman exec` that times out is reported `delivered:false` and turns an
undecided load into `state:"unknown"` with "do NOT re-run blindly" instead of "re-run" [M6]; and
every write-payload + vtmsend pair is serialized by the server-wide `HammerState::vtm_gate`, because
`vtmsend.exe` reads ONE fixed payload path [M7]. The load marks `note_external_job` (a **non**-bridge
job: `control.status` lists no `running` entry for it, so the plain `note_job` grace was being cleared
and the frozen frame loop reported `HammerBlocked`) and releases it when the load settles, so the
blocked gate really says `EngineBusy` for the window — an engine load, not a modal dialog
[review-1 L1]. It holds **no** write gate and sends no `expectDoc`: it changes no
document state, and holding the gate would block real edits for the length of a load.
**The document while the map runs**: the defuse playtest loaded `defuse_poc` in the `cs2tools`
session with that document open and read the same `doc.active` (`epoch:1`, `undoDepth:48`,
`dirty:false`) before and after — the load does not touch it [V, HANDOFF §Playtest steps 1 and 6].
What a bridge request sees *during* the load (the frame loop is frozen, so reads are expected to
stall) is **unverified live** — the orchestrator verifies it; `note_external_job` is what keeps that
window from being reported as a dialog. Tests: `tests/run_map.rs` (38: the happy path with an old
console session's identical success line before the send-time offset, one post per mode line, the
`game.local` remount, the disconnect-first memory path, a failed remount, explicit path without a
document, the flattened-vpk fallback and its sub-directory refusals (incl. the top-level-source
rule, review-2 M8), failure/timeout/unknown/dirty/
stale, a second call refused `EngineBusy` while the previous load is unresolved, concurrent calls
that must not interleave their posts, the `EngineBusy` window and its release, the read-back budget,
the unload route — ok with the memory cleared, the `game.local` no-op, the timeout, the
`delivered:false` failure, the rejected arguments and `unload:false` — the default watch that fits
the budget silently plus the explicit cut that is reported, the
`VpkNotFound`/`RuntimeUnavailable`/`InvalidArguments` refusals that must send nothing, and FIX-3:
the `-32024 NoLocalPlayer` answer read as inGame:false (load + unload), a failed post
leaving no record and no lockout, a failed remount whose record settles at the idle line, and the
no-op that keeps an unresolved record) + the
module's unit tests (timeout bounds and the explicit-only clamp flag, the unload argument, the
disconnect-wait note, mode/name validation, the anchored engine line shapes, the idle level, a
replaced console.log, `ConsoleWatch`/`open_at`, and the shared vpk-candidate lookup).
**Compile is done** (WP-C, below). No Hammer stubs remain.

## compile_map (WP-C, branch `hammer/cmp-a`)

**Two routes behind one tool (see "Jobs" below).** When the bridge advertises `job.start` the compile
is a **bridge job** (`route:"bridge"`): same host-side target resolution and gates as everything
below (path validation, the active-document dirty gate, the M9 in-game refusal, `mode:"nav"` needing
a vpk), then `job.start {kind:"resourcecompiler", input:"csgo_addons/<addon>/<content path>",
game:false, flags:[-threads 8 -fshallow -maxtextureres 256 -nop4 -quiet -unbufferedio -noassert
(+ -nav)]}`; the bridge owns the process (its tree, the lingering Wine process, its deadline), so the
container machinery below (tag, orphan scan, tagged kill, log watcher) belongs to the **podman route**
only (`route:"podman"`, the fallback when `job.start` is not advertised, or when the bridge answers
that it cannot run the compiler here: Windows-only / `resourcecompiler.exe` missing /
`AssignProcessToJobObject` failed - a note says so). The `compile_map {jobId}` id space (`c<N>`), the
result shape (`running|ok|failed|timeout|cancelled`, elapsed, summary, `vpkPath/vpkForm/vpkUpdated`,
`logTail`, `failLines`, `fatal`, `sourceChanged`) and the verdict are the same on both: on the bridge
route the state comes from `job.status` folded into the store entry on every poll (and before a new
start, so a job that ended unpolled does not block it), the vpk/mtime verdict is the host's
(`ok_verdict`, shared with the podman OK path), `log` is the bridge log mapped to the host
(`<app root>/<log.relative>`) and `bridgeJobId` names the `j...` id. `compile_map {jobId, cancel:true}`
ends a job on either route: bridge -> `job.cancel`; podman -> the run task ends the exec and runs the
tagged kill for that job's tag, the job closes `cancelled` with its `cleanup` report; a finished job
answers its final state with `cancelled:false`. Shutdown: bridge-route jobs still running get a
best-effort bounded `job.cancel` (`compile_map::shutdown_cleanup`, `report.bridge`); the orphan `/proc`
scan and the tagged kills apply to the podman route only.

The map compile is the proven headless route (`~/.tmp/defuse-poc/compile_map.sh` + its
HANDOFF.md "Compile + playtest (worker)", proven live 2026-10-13): `resourcecompiler.exe
-threads 8 -fshallow -maxtextureres 256 -nop4 -quiet -unbufferedio -noassert [-nav] -i
/app/content/csgo_addons/<addon>/<path>` execed in the tools container (`CompileTarget::
ANY_TOOLS`, `--tools-container` override) — never Hammer's map.compile/F9 dialog, and with
**no `-game`** (the recipe proven live leaves it out; the addon layout resolves the paths —
the model/particle file routes do pass `-game`). The command is prefixed with
`export CS2MCP_COMPILE_JOB=<tag>;` (review-2 N1): because Wine children inherit the environment, the
container-side cleanup can afterwards kill EXACTLY this job's process tree (`resourcecompiler.exe`
and its `vrad3.exe` children included) instead of the recipe's `pkill -f resourcecompiler.exe`,
which would also sweep a concurrent `model_compile`/`particle_compile` and never reach `vrad3`.
The TAG VALUE is unique per process (`<jobId>-<pid>-<hex nonce>`, `tag_value`): the short
`jobId` is only the tool's poll handle and is NOT unique across hammer-mcp processes (two
servers can both run `c1`, and a restart re-uses ids) — the orphan check may classify a live
tag as this server's own ONLY on an exact tag-value match, and the scripts read the environ
ANCHORED (`grep -E -m1 '^CS2MCP_COMPILE_JOB='`, so a bystander's
`OTHER_CS2MCP_COMPILE_JOB=…` cannot read as a foreign tag).
The cleanup (`tagged_kill_script`, one `exec` after the exec is ended; the script also
batch-kills several exact tags in ONE exec for the shutdown) SIGTERMs the `/proc`
entries whose environment carries the tag, waits 1 s, then SIGKILLs them; `wineserver` and
`services.exe` are EXCLUDED by `/proc/<pid>/comm` (a compile that starts Wine in the container
would otherwise tag the Wine bootstrap — a Hammer started during the compile would attach to the
tagged `wineserver` and die at cleanup, and the tagged wineserver would look like an eternal
orphan); when nothing
matches nothing is killed and the job's `cleanup` report says so. The reported `remaining`
count ignores ZOMBIES and is re-polled for up to 2 s after the SIGKILL (the live lesson:
a `SIGKILL`ed tree reported `remaining=1` that a scan a moment later disproved — the entry
exists only until the parent reaps it). The output candidates are
shared with `run_map` (`compile_map.rs`'s `pub(crate) vpk_candidates(app_root, addon,
content_path)`, review M8): the authoritative location for a sub-directory map is unverified
live, so both spellings are carried — the compiler's content→game shape `maps/sub/x.vmap` ->
`maps/sub/x.vpk` and the recipe script's flattened `maps/${MAPNAME##*/}.vpk` — and the verdict
reports the one whose mtime advanced (`vpkPath` + `vpkForm`), or both (`vpkCandidates`) when
neither did. The job also records the source `.vmap`'s mtime at start and every poll answers
`sourceChanged` when it moved under the running job (review L2: a `save_map` during the
compile is flagged, not refused — `vrad3` may reread it). The other half of L2 is on the writers'
side: `save_map`/`reload_map` ask `CompileJobs::running_for_path` for a `Running` job of the map
they just wrote and, when there is one, add a **note** to their result (never a refusal) saying the
compiler may reread the `.vmap` for its later passes, so the job's output need not match the file —
re-run `compile_map` after it ends.
**The job does not outlive the server** (the live orphan): `main` races `serve_stdio` against
SIGTERM/SIGINT (`cs2_mcp_common::mcp::termination_signal`, on the same path as stdin EOF) and runs
`shutdown_cleanup` first — the tagged kill for EVERY job left without a cleanup report (a
`Running` one, but also the probe-error / exit-fold / supervisor / dead-reaper paths' `failed`
jobs whose tagged children may still be alive, review-2 N5; the exit-fold Failed path runs the
kill itself, so most jobs already carry a report), batched into ONE kill script per container
when several jobs need it; bounded at `SHUTDOWN_KILL_BUDGET` (5 s), reported in the log; the
cleanup FLAGS the store shutting down first (review-2 N7: `CompileJobs::start` refuses under
`StartRefusal::ShuttingDown`, so a start racing the shutdown cannot insert + spawn a job that
would be orphaned by the exit), and a signal exit goes through `std::process::exit(128 + <signal>)`
(the conventional 143/130, so a supervisor can see that a signal got the process — N7), because
rmcp's blocked stdin reader would otherwise keep the runtime's drop waiting for ever (the
live test `tests/stdio.rs` covers both endings over the real binary). At the **next start** the same
tag is the orphan check: ONE bounded exec scans `/proc/*/environ` for
`^CS2MCP_COMPILE_JOB=` (the game's own processes answer `Permission denied`; the read is
redirected, so they stay silent and are
not counted) and reports every tag alive. A tag whose VALUE this server knows (a finished/reaped job
whose tree outlived its verdict) is ended with the tagged kill — the tag is ours; a **foreign** tag
refuses the start (`EngineBusy`, `data.reason:"foreign_compile_job"`, the tag named and the other
compile pointed at) instead of stacking a second compiler on the same output, and is never
auto-killed. An unreadable scan is a note, not a refusal (`orphanKills` carries the own-job kills on
a start that went through).
The Wine process lingers after the ` OK: N compiled, 0 failed, M skipped, Tm:Ts` line (leading
space + CRLF), so the job uses `DockerRuntime::exec_in_background` (`cs2-mcp-common`) and
watches the log at 500 ms for the recipe's `^[[:space:]]*OK: [0-9]+ compiled` line; the watch
reads LOSSY and INCREMENTALLY (review M1: `fs::read` + `from_utf8_lossy` from a running offset
— one non-UTF-8 Wine byte can never blank the log into a 25-minute false timeout); on the OK
line the exec is SIGKILLed (the recipe's observed exit-137 teardown; the exit status is
worthless) and the vpk + the shared `parse_compile_output` summary decide the verdict. When the
exec EXITS, the log is read once more (review M2: a clean exit that raced the poll's read by
microseconds still takes the OK path, not a false `failed`). A process exit without the line
is `failed`; the deadline (1500 s default, `timeoutMs` up to 3600 s) ends it `timeout` the same
way (both record the `cleanup` report); the transient `crun: … status: No such file or
directory` exec failure is retried exactly once, and `ExecHandle::finished` answers an `Err`
(cs2-mcp-common, M4) that closes the job instead of panicking the watcher.
It is a **background job** (`HammerState::compile_jobs`, bounded at 8, at most one running per
server): start returns `{jobId, state:"running", log, vpkCandidates}` at once and
`compile_map {jobId}` polls (`running|ok|failed|timeout`, elapsed, the OK summary, vpk
path/form + `vpkUpdated`, log tail, failLines, `sourceChanged`, `cleanup`). The log is ONE FILE
PER JOB, `<repo>/target/compile-<map-dashed>/rc-<jobId>.log` (review L3: `maps/a-b.vmap` and
`maps/a/b.vmap` collapse to the same dashed name, so a new start must not delete an older
job's poll view). The one-at-a-time rule is race-free WITHOUT the write gate (review L7):
`CompileJobs::start` does the check-and-insert as ONE operation on the store's own mutex —
the server-wide write gate stays free for counted writes. The spawned run task is supervised
(review M4): a panic in it closes the job `failed` (`failed (watcher panicked)` wording in the
reason), and a job running past deadline + a 120 s reap grace is reaped the same way the next
time anything reads the store — a lost watcher can no longer refuse every later start until
restart. The target: `path` (tool-validated like `open_map`'s) or the active document's map;
the tool compiles the SAVED file, so the active-document target is refused `-32016
UnsavedChanges` while the document is dirty, dirty-unknown, or this server counted unsaved
edits (`save_map` first); `mode:"nav"` needs a compiled vpk AT ANY CANDIDATE location (M8) and
adds the recipe's `-nav` (EXTRA_FLAGS, only nav + map resource, the world from the existing
vpk). When the engine holds the target's compiled `.vpk` mounted — the live `game.local` answer
(read through the SAME `read_game_local` helper `run_map` uses, so it connects first and
resolves the live `-32024 NoLocalPlayer` as `inGame:false`), or, without a bridge `game.local`
at all, this server's own last OK load of the map (the H1 fallback, review-2 M9) — the start
is refused `EngineBusy` with an
`run_map {unload:true}` hint (`data.hint`) — a differently-mounted engine is not a refusal, and an
`game.local` read that fails is a note, never a refusal. The bare-stem mounted name (`x` for
`maps/sub/x.vmap`, the flattened spelling) refuses only when it is UNAMBIGUOUS: a top-level
`maps/x.vmap` source next to the target means the engine's `x` could be THAT map's compile, so
the check adds a note instead of refusing (review-2 M9's false-refusal fix). Mutability
`Writer` (it writes the vpk), no
bridge methods (the bridge is only used for the default map, the dirty gate and M9 — without a bridge
an explicit-path start proceeds with a note). `src/tools/compile_map.rs`; tests
`tests/compile_map.rs` (30, fake podman per behavior: ok (+tagged-kill argv shape and the orphan
scan), flattened and content-relative sub-directory outputs, nav over either candidate, non-UTF-8 log
byte, reported failures, death-no-OK, timeout, linger, crun retry, in-game refusal + unreadable
`game.local` note + the NoLocalPlayer-not-in-game answer + the no-`game.local` load-memory refusal,
moved-source flag, busy/race, refusals, dirty, finished-poll, the foreign-tag
refusal, the own-tag kill, the unreadable scan note, the shutdown cleanup (Running, closed-without-
cleanup, and the batched single-container kill) and the per-process tag shape) and the module's unit
tests (ok-line shape, log-lossy incremental reader, exit-fold M2, deadline bounds, dirty gate, vpk
candidates/choice, mounted-name match incl. the ambiguous bare stem, tagged-kill script (incl. a
host `bash` run that kills only tagged sleepers and reports the unreaped zombies as remaining=0),
the tagged-scan script/report/classification, the per-process tag value, the shutting-down refusal
and the store's needing-cleanup/running-for-path/reap/evict lists), plus `tests/stdio.rs` end to
end over the real binary (stdin EOF and SIGTERM both leave the tagged kill for a job that was still
running; the SIGTERM exit is the conventional 128+15). **Live behaviour is the
orchestrator's** (the linger/kill/vrad3 timing were never re-run from this code path; the
authoritative sub-directory vpk location stays unverified until then).

## Jobs (bridge `job.*`: compile_asset, job_status, job_cancel)

The calling model should not care how resourcecompiler.exe runs. `src/tools/bridge_job.rs` is the one
helper over the bridge's job model (`job.start`/`job.status`/`job.cancel`, kind `resourcecompiler`,
wire contract `target/orch/unblock-rows/jobs-contract.md`): `probe` (connects lazily THEN asks `has`,
like `read_game_local`), `start`/`status`/`list`/`cancel` (ungated requests: the bridge answers them on
its IPC lane while Hammer's main thread sits in a modal), `wait` (poll 250-500 ms to a deadline, cancel
on the deadline), `host_log` (`log.relative` -> `<app root>/<relative>`) and `map_error`:
`-32017 "job: <id> running"` -> `EngineBusy` (`data.jobId`, `data.reason:"bridge_job_running"`), any
other `-32017` -> `RuntimeUnavailable` with `data.blockedOn`, `-32602` -> `InvalidParams`.
`route_unavailable` recognises "this bridge cannot run compile jobs here" (Windows-only, compiler not
found, `AssignProcessToJobObject`): `compile_map`/`model_compile`/`particle_compile`/`compile_soundevents`
fall back to the podman route with a note, `compile_asset` refuses. Compiles never call `note_job`
(a compile does not freeze the frame loop), and the client's job-grace check ignores
`resourcecompiler` jobs in `control.status`.

* `model_compile` (route resourcecompiler), `particle_compile`, `compile_soundevents` all go through
  `compile::compile_resourcecompiler`: with `job.start` advertised it starts a bridge job
  (`game:true`, `flags:["-nop4"]`, the input from `ContentPaths::content_rel`), polls to terminal,
  cancels on the tool's timeout (the bridge gets a 5 s backstop deadline beyond ours), then keeps the
  host-side output-mtime check. The result keeps its podman-route fields and gains `compiler:
  "bridge"|"podman"`, `jobId` (`null` on podman), `state`, `timedOut`, `log` (host path). `ok:false`
  stays a result.
* `compile_asset {path, game?, flags?, timeoutMs?}` (Writer): any asset of the active addon the bridge
  allowlists (vmap vmdl vpcf vsndevts vmat vtex vsnd vsmart vanim vmks); `path` addon-relative,
  `csgo_addons/<addon>/...` or an absolute/Windows host path inside the addon tree (resolved with
  `ContentPaths::resolve_read_ext`; outside -> `WriteOutsideAddon`, missing -> `AssetNotFound`);
  `game` defaults to false for `.vmap`, true otherwise. Returns at once `{jobId:"j...", state:"running",
  route:"bridge", path, input, game, flags, log (host), command, next}`. No podman fallback: no bridge
  / no `job.start` / a bridge that cannot run the compiler -> `RuntimeUnavailable` naming `job.start`
  (`data.missing`) or the bridge's `blockedOn`. For a MAP use `compile_map` (gates + vpk verdict).
* `job_status {jobId?}` (ReadOnly): `j...` -> the bridge status plus `log.host`, `route:"bridge"`, and
  `compileMapJobId` when a `compile_map` job owns it; `c<N>` -> exactly `compile_map {jobId}`; none ->
  `{bridge:{jobs:[...]}|null (+bridgeNote), compileMap:[...]}`.
* **`waitMs` (live-fix D).** `compile_map {jobId, waitMs}` and `job_status {jobId, waitMs}` wait for
  the job instead of returning at once (acceptance run 2: an agent that may not sleep polled
  `compile_map {jobId}` 2019 times). The call returns when the job leaves `running`, or after
  `waitMs` (0..=600000, cut to the tool budget minus a 1.5 s reply margin; `budgetClamped:true` when
  the budget did it), whichever is first; the reply gains `done` and `waitedMs`, and a job still
  running gets a `next` that calls the same tool again with `waitMs:30000`. **Default `waitMs` is
  30000** (review fix 2): a bare `{jobId}` waits, because a weak agent calls the tool with the
  arguments it remembers (it called the bare shape 2019 times); `waitMs:0` is the explicit "answer
  at once". A finished job still answers on the first poll. Each poll's request deadline is cut to
  what the tool budget leaves after the 1.5 s margin (`bridge_job::status_within`), so a bridge that
  stalls on the last poll cannot overrun the call. `waitMs` without a `jobId` is `InvalidParams`; `job_status {}` (the list) ignores
  it. **There is no completion signal to wait on** (a bridge job ends inside the bridge and only
  `job.status` says so; the podman run task updates the store but has no notifier), so the wait is
  a poll loop INSIDE the server: `jobs::wait_for_job`, one view every 500 ms (`WAIT_POLL_INTERVAL`)
  against a hard deadline, a poll error ends the wait. Only these two tools return a job to poll;
  `model_compile`/`particle_compile` are synchronous. Tests: `jobs::wait_tests` (ends on
  completion before the deadline, ends at the deadline with the next step, budget clamp, error,
  validation).
* `job_cancel {jobId}` (Writer): `c<N>` -> `compile_map {jobId, cancel:true}`; `j...` owned by a
  compile_map job -> the same (the store follows); other `j...` -> `job.cancel` + the final status with
  `cancelled`. A finished job answers its final state with `cancelled:false`; an unknown id is
  `InvalidParams` ("unknown jobId").
* Ids: `c<N>` (compile_map, this server) and `j<N>-<nonce>` (bridge, opaque, unique across bridge
  loads). The bridge runs one job at a time. Every job this server started (compile_map bridge jobs
  and compile_asset ids) is cancelled at shutdown (`report.bridge`): a job does not outlive the server.
* Lost replies and races (review fixes): every `job.start` this server sends carries a unique
  `clientRef` (`hm<pid base36>-<counter>`), which `job.status` echoes. The store entry is inserted
  before the request goes out. Only definitive refusals (-32017/-32602/-32603) and errors raised
  before the request was written (no bridge, handshake failure) roll back; after a transport error
  the entry is kept with an empty bridge id and the next poll/cancel/start adopts the job from
  `job.status {}` by `clientRef` ONLY (no input/time heuristics: another caller's job for the same
  input is never taken). `compile_asset` and the file-route compiles register a `clientRef`
  placeholder (`CompileJobs::note_ref`) BEFORE sending; any `job.status {}` (a `job_status` list,
  the shutdown) resolves it to the job, so a lost reply or a dropped future never orphans a job.
  A `StartGuard` counts starts in flight: shutdown waits up to `SHUTDOWN_START_WAIT` (3 s) for
  them, then gives the cancels their own `SHUTDOWN_KILL_BUDGET`; a start whose reply arrives during
  shutdown cancels the new id. A cancel in the start window picks the route from the entry
  (`bridge_ref`), marks `pending_cancel` in the same locked step that reads the id
  (`bridge_id_or_pend_cancel`) and answers `cancelRequested:true`; it is applied when the id
  arrives. An id-less entry whose bridge stays unreachable past `ADOPT_GIVE_UP` (30 s) is closed
  failed "outcome unknown (bridge unreachable)"; a bridge entry running past deadline + grace is
  closed the same way, but ONLY by a sync path that just failed to reach the bridge (a plain store
  read never reaps it), and a later terminal `job.status` overwrites that guess (`BridgeRef.reaped`).
  "job: another job is starting" is `EngineBusy` (`bridge_job_starting`, retry_safe). File-route
  waits record their id (shutdown), cancel once on a status error, and cancel on drop; a failed
  timeout-cancel is reported as `cancelError`.
* Tests: `tests/bridge_jobs.rs` (41, against the mock bridge's scriptable `job.*`,
  `cs2_mcp_common::mock::jobs`: ok/failed/fatal/timeout/hang, unavailable, busy) and the route
  fallback/podman-cancel tests at the end of `tests/compile_map.rs`. Nothing here ran live.

## Props (WP-M/WP-V/WP-P/WP-R1/WP-R2)

RE facts (SetKeyValue model load, node transform vslots, I/O vector, live probe evidence):
[research/props-re.md](research/props-re.md).

The prop tools (`hammer/props`) place model entities: `list_models` (the model catalogue),
`get_model_info` (WP-R1, `model_info.rs`: what a compiled `*.vmdl_c` carries),
`list_props` (the props of a map), `create_props` (WP-R1, `props_live.rs`),
`set_node_transform` (`node_transform.rs`) and the two entity-I/O
tools (`entity_io.rs`). The fixed order is `PROP_TOOL_ORDER` in `src/tools/props.rs`; each module
owns its specs.

The shared plumbing is `src/vpk.rs` + `src/tools/model_catalog.rs`, cached on `HammerState.model_catalog`.

### list_models (WP-V, branch `props/wp-v`)

Read-only, no bridge methods (pure filesystem). Lists every model a prop may name as a
canonical `models/**.vmdl` path (the `model` keyvalue spelling: lowercase, `/`, `.vmdl` even
when the file is `.vmdl_c`), from three sources in precedence order —
`game/csgo_addons/<addon>/models/**/*.vmdl_c`, `content/csgo_addons/<addon>/models/**/*.vmdl`,
then the game VPKs `game/csgo/pak01_dir.vpk` and `game/core/pak01_dir.vpk` (read through
`src/vpk.rs`, a std-only VPK directory parser: v1+v2 header, tree walk, `0x7FFF` in-dir data,
sibling archives, preload bytes, 256 MiB read cap; every read bounds-checked to `VpkError`,
never a panic). The precedence is real data, not just wording: one canonical path present in
two sources is ONE entry whose `sources` reads `[addon-game, …, vpk:*]` (sorted by rank after
assembly) and `read_compiled` walks exactly that order — an addon recompile of a stock prop is
read from the addon, never from the VPK. Each walk takes only its own source's extension
(compiled dir `.vmdl_c`, content dir `.vmdl`), matched ASCII-case-insensitively (`Foo.VMDL_C`
is cataloged; a stray `.vmdl` under the game dir or `.vmdl_c` under content is not), and
`ModelSource::AddonGame{file}`/`AddonContent{file}` keep the ON-DISK spelling so a read opens
the file the walk saw (a lowercase-only rebuild missed mixed-case names on ext4). Args:
`query` (substring, or glob when it carries `*`/`?`), `prefix` (`models/`-rooted), `source`
(`any`|`addon`|`game`), `compiledOnly`, `limit` (1..=1000, default 100), `offset`;
unknown/ill-typed/out-of-bounds args are `InvalidArguments`, nothing dispatched. The catalog is built once inside `spawn_blocking`, cached on
`HammerState.model_catalog`, refreshed when the csgo VPK's mtime moves, and a **lookup miss
rescans only the addon dirs** (addon files appear mid-session) and stores the rescan. Missing
or unreadable state is never a silent empty list: a missing game VPK is `VpkMissing` naming the
file (also `CatalogVpkError` for a parse failure), an absent addon directory / a walk that hit
its 16-directory or 200k-file bound / an unreadable directory land on `note` in the result,
symlinks are never followed, and `get`/`read_compiled` normalize
(`normalize_model_path`: absolute/`..`/non-`models/`/bad-extension spellings refused) and error
explicitly (`ModelNotFound`, `ModelNotCompiled`) — `lookup_model`/`read_compiled_model` are the
module API `get_model_info` (WP-M) codes against. Tests: `tests/vpk.rs` (synthetic v1/v2 VPKs,
malformed corners by patching bytes; the two `#[ignore]`d corpus tests open the real install's
paks — csgo tree 6277806 bytes / 137693 entries, 12715 `models/*.vmdl_c`, and one read model's
u32 size == len with u16 version 12, re-verified 2026-10-16) and `tests/model_catalog.rs`
(fixture app tree with synthetic VPK pair + addon files under `CARGO_TARGET_TMPDIR`:
precedence/merge, normalization, filters, paging and bounds, the missing-VPK two-pak errors,
absent-addon note, depth bound + symlink, miss-rescan flow, the addon-overrides-VPK precedence
test (`an_addon_copy_outranks_the_game_vpk`), the per-source-extension/case walk test
(`the_addon_walks_take_their_own_extensions_and_any_case`), and the `list_models` argument
negatives). Unverified live: nothing here touches the bridge; concurrent-build races resolve
last-writer-wins (both builds are correct catalogs).

### list_props (`src/tools/props_query.rs`, WP-P)

`list_props` (ReadOnly, no declared bridge methods — a file read never needs the bridge): prop
entities of a map from the SAVED `.vmap` (route `file`) or the live document (route `live`,
bridge m2.15 `entity.find` + one `entity.get` per candidate). A prop is a `CMapEntity` whose
FGD class carries a prop tag — `src/fgd` gained `PropTag`/`prop_tag_from_args` (the
`tags( PropStatic|PropDynamic|PropPhysics )` args of one helper) plus
[`Database::prop_tag`] (the class first, then its resolved base chain, first tag wins) and
[`Database::declares_model_studio`] (a resolved `model` key of type `studio`) — both additive,
fixture tests on the tag parser in `src/fgd/mod.rs`. Kinds: `static`/`dynamic`/`physics`;
`kind:"any"` lists them all. `includeOtherModelEntities:true` adds classes not prop-tagged but
declaring `model(studio)` (kind `"other"`), and non-FGD classes whose entity carries a
non-empty `model` keyvalue (kind `"unknown"`, `problems:["unknown class"]`) — an unknown class
without a model keyvalue stays out (stated rule, pinned by tests). A missing/empty `model`
keyvalue is `problems:["no model"]`. The file route reuses the entities-file plumbing
(`resolve_document`/`stale_read`/`source_json`): same `path` argument rules and error codes,
`staleRead`/`staleReason`/`unsavedBridgeEdits` envelope, `file` object. The FGD is REQUIRED on
both routes (`FgdUnavailable`/`FgdParseError`, never an empty success). Result envelope:
`total`/`returned`/`offset`/`truncated`, `summary.byKind` (all five kinds, zeros included) +
`summary.byModel` (top 50 by count then name, group case-insensitively with the first-seen
spelling — model paths are case-insensitive text here, the path map's rule), props in stable
scan order (file: document element order; live: the bridge's walk order), `includeKeyValues`
adds the read keyvalues (live: the `entity.get` reply's `keyValues`).

Verified by `tests/props_query.rs` (fixture .vmap GENERATED in the test from the committed
`probe.vmap` by same-length classname patches — `func_buyzone`→`prop_dynamic`,
`info_target`→`prop_static`, `end_of_match`→`prop_physics`, occurrence counts pinned; a
hand-written fixture FGD carries the tags and the inheritance) and via the mock bridge for the
live route: kind totals/order/paging, `within` box (the bridge's `within` is a sphere, so the
live route computes the box locally), class/targetname exact case-insensitive, model substring
case-insensitive, byModel ordering, unknown-kind inclusion/exclusion, `NoDocument`/
`FileNotFound`/`BridgeNotRunning`/`FgdUnavailable` + the argument negatives (limit 0/2001, bad
kind, `offset<0`, unknown argument incl. inside `within`, min>max box, empty filters,
`source:"live"`+`path` contradiction — each refused before any file read or bridge send), the
live `entity.find` error passthrough (`DocumentChanged`) and an `entity.get` failure rendered
as `problems:["live read failed: …"]` rather than invented fields; `#[ignore]`d corpus test on
the REAL game tree with the REAL FGD (oracle-verified numbers: smartprop_zoo 102 props 41
static/4 dynamic/57 `prop_physics_multiplayer` — tag inheritance several `base()` hops deep;
cs_script_demo 17 props 7 static/10 dynamic; none without a model).

Live-route bounds, deliberately (documented in notes, never silent): ONE `entity.find` at the
bridge's full 2000-row page (no offset exists in the bridge method — a larger document is
`truncated` with a note); the file route reads everything, but live keyvalues need one
`entity.get` per prop, so the examine pass caps at 512 candidates (`notes` names the cut). The
512 window is filled only by candidates the caller's filters keep: `kind` and `within` are
decidable on the `entity.find` row itself (the classname carries the kind, the origin rides
the row) and are applied before the window counts them — a map with more excluded candidates
than slots still finds the wanted props, and no bridge read is spent on excluded rows; the
note cut only ever counts kept candidates. `path` must be a string when present (a
non-string one is `InvalidArguments` in the argument phase — never silently routed as
"no path"); `kind` accepts `other`/`unknown` too (they match only under
`includeOtherModelEntities:true`). Live `scales` is the **node field** the m2.18 `entity.get`
reports (`review W2 3c`): the value comes from the read, `null` only for a payload without a
usable one (an older bridge, or a failed read) **and** a note counts those — never an invented
`[1,1,1]`. `list_props` itself is NOT yet verified live (the orchestrator runs the live pass; the
m2.15+ reads underneath it are verified live).

### get_model_info (WP-R1: the tool over the WP-M library)

`get_model_info {model, include?, limit?}` is the **tool** over the WP-M **library**
`hammer_mcp::resource` (`model_info_from_bytes` is the same parse the tool runs — no second
parser). The tool is ReadOnly (no declared bridge methods: catalog + `.vmdl_c` bytes are local
files), `src/tools/model_info.rs`. Args: `model` in any spelling `normalize_model_path` accepts
(case/backslashes/`.vmdl_c` all spell one `.vmdl` path), `include`
`["sequences"|"attachments"|"bodygroups"|"keyvalues"]` (default: all lists but the embedded
KV3 text — `keyvalues` opts in), `limit` 1..=2000 per list (default 200; a cut is reported in
`truncated.l` plus a note — all three lists ship at most `limit` entries after review w2b #4:
bodygroups honour it too, not just sequences/attachments). Result: `model` (canonical),
`sources`, `compiled`, `name`,
`bounds.render`/`bounds.physics` (each `min/max/size/center`; all-zero or missing → `null`
**with a parser note**, never an invented box), `hull`/`view` (m_modelInfo's raw min/max),
`mass`, `surfaceProperty`, `hasPhysics`, `skins`, `bodygroups` (exporter names merged into
named groups with `choices[]`/mask/bits; an `auto:true` entry with `choices:null` is the
no-authored-group case — no group name is invented), `sequences`, `attachments`,
`placement{floorOffset, footprint}` (floorOffset = `-render.min.z`: how far the origin moves up
so the UNROTATED model rests on a floor at its z; footprint = render x/y size; a tilted prop's
lift is `snap_floor_lift`'s rotated/scaled box, not this number). `placement` carries `null`s +
notes when the render bounds are missing.
Errors name the model: unknown → `ModelNotFound` (≤5 catalog suggestions: same directory
first, then a 3+-character shared name stem ranked by shared-prefix length — review w2b #10 —
with the alphabetical tiebreak), cataloged-but-uncompiled addon → `ModelNotCompiled` with the
`model_compile` hint, unreadable/decode failure → `ModelRead` naming the model — never empty
facts on a corrupt read.

The library itself ships unchanged (the section text below is WP-M's, kept for the format facts):

This work package ships the **library** only: `src/tools/model_info.rs` stays a placeholder and a
later WP wires the tool. `hammer_mcp::resource` (mod.rs) parses the compiled-resource container,
`resource::kv3bin` the binary-KV3 blocks, `resource::model` the facts:

```rust
let info = hammer_mcp::resource::model_info_from_bytes(&bytes)?;   // bytes of one *.vmdl_c
```

Read: the 16-byte container header + 12-byte block table (offsets relative to their own field, every
block range-checked), `DATA` (the model's KV3 tree — a missing or undecodable `DATA` is a hard
error), and the repeatable `MDAT`/`ASEQ`/`PHYS` blocks (a secondary block that fails to decode adds
a `notes` entry instead of failing the model). `ModelInfo` carries `name`, `hull`/`view`/`mass`/
`surface_property`/`keyvalues_text` (`DATA.m_modelInfo`), `skins` (`m_materialGroups[].m_name`),
`bodygroups` (`m_meshGroups` + `m_refMeshGroupMasks`, split into group/choice), `sequences` (ASEQ),
`attachments` (`MDAT.m_attachments`), `has_physics` (a `PHYS` block or non-empty `m_refPhysicsData`),
`render_bounds` (union of `MDAT.m_sceneObjects[]`), `physics_bounds` (union of the `PHYS` shapes),
`blocks` and `notes`.

Verified by decoding **every** block of all 13 105 `*.vmdl_c` in the installed
`game/csgo/pak01_dir.vpk` (66 334 KV3 blocks, 10 189 with binary blobs): KV3 **versions 4 and 5**
and **compressions 0 (none), 1 (LZ4), 2 (ZSTD)** all occur and all decode. Notes on the format are
in `kv3bin.rs`'s module doc (blob tail `[lengths][trailer][u16 frame sizes]`, LZ4 frames chained into
one output stream across blobs, buffer-2 lanes aligned against the buffer start, ZSTD frame groups
holding the version-4 buffer + blobs as concatenated frames). Hostile inputs stay bounded:
object-member dedup is a name→slot map (the old linear `find` per member was quadratic and let
a million-member object pin the decode thread for tens of minutes), the header-length check
runs before any lane read (`HeaderTooSmall` for any short block, v4 72 / v5 120 bytes), and
bytes past an accounted payload are refused (`TrailingBytes`) except the file's own
end-of-buffer trailer — the corpus re-verified after that change (all 66 334 blocks still
decode). On real models most `m_modelInfo` fields are empty (all zero hull/view except the 81 player/agent models, `m_flMass` 0 in all 13 105,
`m_sSurfaceProperty` empty in all): the usable boxes are `render_bounds`/`physics_bounds`, so callers
should prefer those and read `notes`.

Not verified / open: the sampled install carries no *uncompressed* block with blobs (the decoder
refuses frame sizes there) and no v5 ZSTD block with LZ4-style frame sizes; `MVTX`/`MIDX`/`MBUF`/
`RERL` blocks are not decoded (not KV3); KV3 versions 1–3 and the legacy `VKV\x03` container are
refused with a clear error (the model compiler does not emit them). Tests: `tests/resource.rs`
(synthetic containers/KV3 for both versions × all three compressions, the negatives, the `#[ignore]`d
`corpus_models_decode` — 200 models by default, `CS2_MCP_CORPUS_MODELS=N` widens it — and
`corpus_zstd_blocks_decode` for the install's 42 ZSTD blocks).

Verified by the tool's `tests/model_info.rs`: a fixture VPK + addon tree (the synthetic
`.vmdl_c` containers of `tests/common` — the KV3 v4-uncompressed encoder factored there), the
fact shape, `include`/`limit`, the notes on all-zero hulls and missing boxes, the negatives
(bad args, unknown model + suggestions, uncompiled + `model_compile` hint, decoy bytes →
`ModelRead`) — the `#[ignore]`d corpus test reads the real
`models/generic/terrace_set_01/terrace_chair_01.vmdl_c` and checks `floorOffset ==
-render.min.z` plus a `[x,y]` footprint. Everything is reviewed against the corpus numbers
WP-M recorded above; the machine-facing shapes were never seen live on an m2.18-era install
(the orchestrator runs the live pass).

### create_props (WP-R1, branch `props/wp-r1`)

`src/tools/props_live.rs` `create_props {props[1..=64], allowUncompiled?}` — **Mutating**,
declares `edit.batch` + `entity.create`. Each prop: `kind` static|dynamic|physics →
`prop_static|prop_dynamic|prop_physics`, or an explicit `class` that must carry a prop tag
(`fgd::prop_tag`, `NotAPropClass` otherwise — create_entities is the general tool), exactly one
of the two; `model` (required, resolved through `model_info::resolve` — the same catalog terms
as get_model_info: unknown → `ModelNotFound` + suggestions; an uncompiled addon model →
`ModelNotCompiled` unless `allowUncompiled:true`, which places it with a per-prop note that
Hammer does not draw it until the addon compiles and the document reloads);
`origin` (required), `angles?`, `scales?` each in (0, 1000] (the m2.18 builder's own bound,
refused locally pre-send), `skin?`, `targetname?`, `keyValues?` (other FGD keyvalues —
`model`/`classname`/`origin`/`angles`/`scales`/`skin`/`targetname` inside `keyValues` are
refused: the fields above are their validated spellings; an FGD **error** refuses the whole
call with nothing sent — an invalid prop is never knowingly created; unavailable FGD → note),
`snapToFloor?` (origin.z lifts so the model's render box rests on a floor at the target z —
the lift is the lowest of the box's 8 corners, each scaled per axis and rotated by the prop's
`angles` with the Source QAngle convention (`model_info::snap_floor_lift`, review w2b #1: the
old `-render.min.z · scales.z` lift put a tilted prop several units into the floor
silently); yaw-only placements give exactly `placement.floorOffset × scales.z`; reported per
prop as `floorOffsetApplied`; no render
bounds and no compiled file share the same `ModelBounds` refusal kind (review w2b #11) — never
a silent skip).
One `edit.batch` of `entity.create` ops (`{class, origin, angles?, scales?, keyValues}`, exact
BRIDGE-M2.18-SPEC §1/§2 shapes — defaults are never sent empty), one bridge undo group
(`MCP: batch`), `expectDoc` from the pre-write read, `note_unverified_edit` mesh-cache rule,
unsaved-edit counter, the m2.17 `doc` tail not echoed. A bridge that does not advertise the
m2.18 marker `node.setTransform` refuses any scales-bearing prop `BridgeTooOld` (-32011,
nothing sent, before the catalog is even read); without scales an old bridge still creates —
with a note that the raw keyvalue write does not draw the model until the document reloads
(re-props.md Q1). Result: `count/requested/created[{index, class, nodeID, origin, angles, scales, handleIssued, kind,
model, floorOffsetApplied?, warnings, notes}], undoLabel/requestedUndoLabel, unsavedBridgeEdits,
saved, fgd, warnings, notes`. `nodeID` is the created node's id from the bridge's create reply
(review W2 3a: a reply from a bridge without it simply has no field here); `angles`/`scales` are
**the transform this call requested**, each `null` when the caller omitted it (review W2 3b) —
`scales` sits next to the bridge's applied echo of the same name. Notes: `prop_static` is baked
into the world at compile
time (compile_map; no targetname/I/O at runtime), dynamic/physics are runtime entities driven
by I/O; no auto-save.

**set_entity_keyvalues model check (WP-R1's second bullet, `entities_live.rs`)**: a `model`
keyvalue on a prop-tagged class is normalized + catalog-looked up before anything is written
(unknown → `ModelNotFound` + suggestions; `force:true`, and a revert — the value equals the
live value — skip the check; a non-prop class keeps the ordinary FGD check). With m2.18 the
same tool's `revertHint` follows the bridge's own `valueUndo` verdict: `"recorded"` (the
RecordNodeTree path) → undo DOES restore keyvalue values (`undo_last restores these keyvalue
values ...`), while `"group-only"`/older keeps the re-apply hint. A `model: ""` value (clear
the model, re-props.md Q1's own workaround) skips the lookup, other resolve refusals say
`pass force:true to write it unchecked` (review w2b #3); with the FGD unavailable a `model`
keyvalue gets a warning that the check did NOT run. `undo_last` (entities_live) STILL uses
the m2.18 itself: a popped `"MCP: set keyvalues"` group on a bridge advertising the marker
`node.setTransform` really restored the values — the notes say so and the unsaved counter
goes down like any other really-undone group; the pre-m2.18 build keeps the
"NOT reverted / counter stays up" wording (both pinned, review w2b #2).

Verified by `tests/props_live.rs` (mock bridge + the fixture FGD + the synthetic-model catalog
tree) — 25 tests: the exact `edit.batch` ops JSON (defaults never sent empty; `expectDoc`,
`deadlineMs`), the world round trip (`keyValues` stored as text, scales in `hammer_scales`), the
static compile note / dynamic I/O note (tag-worded: an explicit class gets NO wrong prop_dynamic
name — review w2b #11), canonical-spelling placement (`MODELS\...\CHAIR.VMDL`),
and the negative list of target/WP.md: kind/class both+neither+unknown+ill-typed, unknown call
argument (incl. the fixed `props[i].class` label) and unknown prop field, `UnknownClass`/
`NotAPropClass` (nothing sent), unknown model
with suggestions + index label, uncompiled-refused and allowed-with-note, scales 0/negative/
above/non-finite/arity/non-number, identity keys refused in `keyValues`, FGD-invalid keyvalues
(undeclared key, integer mismatch), 0 / 65 props (64 exactly passes), old bridge: scales-bearing
`BridgeTooOld` pre-send vs scale-less create with the reload note, snapToFloor (lift × scales.z
/ `ModelBounds` (both nothing-to-measure kinds share the name after review w2b #11) / uncompiled
refusal / decoy `ModelRead`, plus the TILTED regressions — roll 90,
pitch 180, pitch 90 × x-scale 2, yaw-only unchanged against the old buggy lift), partial-failure
`-32012` passthrough
(handles sanitized, counter up) and the `-32603` maybe-executed path (outcome unknown, op really
applied). The `set_entity_keyvalues` model check: unknown model refusal naming `force:true`,
`""` clears the model with the skipped-check warning, and the non-prop-class test observes the
write going through (`build_models()` off, no strict — review w2b #8). `cs2-mcp-common` mock
tests pin the create-time scales round trip (reply echo — now UNCONDITIONAL `[1,1,1]` like the
bridge, review w2b #6 — store
into `node_scales`, undo removes it) and both value-undo shapes (`keyvalue_undo_leaves_the_value_
changed` pre-m2.18 / `keyvalue_undo_records_the_values` m2.18; the `valueUndo:"recorded"` →
`RECORDED_HINT` switch of `revertHint` rides the same knob — the mock knob the review found
dead is now the regression switch, review w2b #5). NOT verified live: everything
against a real m2.18 bridge
(scale vslot draw behaviour, the pre-m2.18 reload
note) — the orchestrator runs the live pass; the shared batch plumbing underneath is the
live-verified `create_entities` one.

### set_node_transform (WP-R2, branch `props/wp-r2`)

`src/tools/node_transform.rs`. **Mutating**, declares `node.setTransform` + `entity.get`, so a
bridge older than m2.18 gates it out with the shared `NotImplemented`/`CapabilityMissing` (the
description names m2.18). Args: `nodeID` (0..=2147483647), `origin?` `[x,y,z]` each within +-1048576,
`angles?` `[pitch,yaw,roll]` finite and inside the bridge's 32-bit float range, `scales?` `[x,y,z]` each in (0, 1000], `relative?` (default
false), **at least one** of the three members; unknown/ill-typed arguments and out-of-range values
are `InvalidArguments` with nothing sent.

Flow (one write gate, one `doc.active` read whose fingerprint rides the write as `expectDoc`): the
node is read first (`entity.get`) — that read is what `relative:true` composes from, and it is made
in absolute mode too so the result can carry the FGD `classname`, `previous` and the `prop_static`
note, and a dead id fails with the read taxonomy (`EntityNotFound`) before anything is sent. A mesh
node answers `-32602 {reason:"not an entity"}`: expected (the tool's second valid class), noted,
and the write continues. Then `node.setTransform {nodeID, origin?, angles?, scales?, expectDoc}`
carries only the requested members. Result: `{nodeID, class (the bridge's node class), origin,
angles, scales (read back by the bridge), classname?, previous? (relative only), undoLabel:"MCP:
set transform", unsavedBridgeEdits, saved:false, notes}`; the m2.17 `doc` tail is consumed by the
cache decision, not echoed.

`relative:true` adds `origin`/`angles` to the node's current values and multiplies `scales` with
them. A requested relative field the read cannot supply is **refused** rather than composed from an
invented base: the m2.18 `entity.get` reports `origin`, `angles` **and `scales`** (the C++ node
fields, `re-props.md` Q2 — the schema text, the module doc and this section said the opposite until
review-2 W2 #4), so relative scaling of an entity works; a reply without a usable `scales` (an older
bridge) is refused for a relative scales call, and any relative field on a mesh node is refused too
(there is no live mesh transform read: `node.list` carries origins only, `mesh.get` is `-32011`).

**Mesh scales read back as `[1,1,1]`.** Live (2026-10-16, `target/orch/live/mesh_probe.log`): a
`set_node_transform` on a `CMapMesh` with `scales:[2,1,1]` visibly stretches the mesh in the editor
render, but the bridge's readback right after it reports `scales` `[1,1,1]` [V]. The probable reason
is that `CMapMesh`'s SetScales (vslot 19) applies the scale to the vertices and leaves the node's
scale field at identity [I: not traced in the binary]. So for a mesh, the `scales` in the reply
does not echo the request, and a second `scales:[2,1,1]` call would likely stretch the mesh again [I]
(it would compound). Entities keep the scale in the node field and read it back as written.

**A transform counts the edit** (`note_unverified_edit`), also on a failure that may have landed; a later mesh operator reads the live geometry again (`mesh.get` converts through the node's own transform), so nothing remembered goes stale. A pre-write refusal (`-32602`/`-32004`/`-32018`/`-32022`) counts nothing. `prop_static` (FGD `tags(PropStatic)`, read from the cached FGD database) gets the
note that the move reaches the editor and the view only until the next `compile_map`.

Tests: `tests/node_transform.rs` (17) — entity/mesh happy paths, the exact request JSON, relative
deltas/readable-scales/composition ranges, the local validation matrix, the read's `EntityNotFound`,
the write's `-32022` passthrough, the class gate, the `-32004` switch, the capability gate, the
compile note, and the three cache rules; `crates/cs2-mcp-common/tests/mock_entities.rs`
(`node_set_transform_shape_and_negatives`) pins the wire-level gates. The relative-scales test uses
the plain mock (`entity.get` reports `scales` since review-2 W2 #4) and a responder without the
field for the refusal path. **Not live-verified**: the bridge side is WP-B's (`node.setTransform` is
`[I: offline]` until the orchestrator's live run — `bridge/test_props.py` covers it and asserts the
origin refusal names 1048576, W2 #5) — nothing here was run against a real Hammer.

### entity I/O (WP-R2, branch `props/wp-r2`)

`src/tools/entity_io.rs`. `add_entity_connections {nodeID, connections 1..64, force?}` (Mutating,
declares `entity.addConnections` + `entity.get` + `entity.find`) and `remove_entity_connections
{nodeID, indices? | match?}` (Mutating, declares `entity.removeConnections`); both need bridge
m2.18. A connection is `{output, target, input, param?, delay?, timesToFire?}` — `output`/`target`/
`input` non-empty <= 255 bytes with no control characters, `param` text (may be empty), `delay`
finite in [0, 3600], `timesToFire` an int >= -1 that is not 0; unknown members (a `delayTime` typo)
and unknown top-level arguments are refused, so nothing is silently dropped. Only the members the
caller gave go on the wire (the bridge's defaults: `param ""`, `delay 0`, `timesToFire -1`,
`targetType 7`).

Before anything is sent, `add` reads the source (`entity.get`, its live FGD class) and resolves
every `target` (`entity.find` by targetname), then checks the FGD: the source class must declare
the `output` (`UnknownOutput` listing its outputs) and a target the live document names must accept
the `input` (`UnknownInput` listing the valid inputs of every class that matched — a name shared by
several entities is checked as a group with a note). `force:true` skips both checks. A target that
is a `prop_static` (FGD `tags(PropStatic)`, i.e. compile-time) is **always** refused
(`StaticPropTarget`): it has no runtime inputs, so the connection could never fire. A target that
names nothing, or starts with `!` (`!activator`/`!self`/`!player`, no lookup is made for those), is
allowed with a note, as is a target class the FGD does not carry. An unavailable FGD is a warning,
never a refusal (`fgd.unavailable`). Result: `{nodeID, classname, added[] (the bridge's own
`outputName/targetType/targetName/inputName/overrideParam/delay/timesToFire` entries, review
W2 #2), count (the entries the bridge's vector CONFIRMED — never the vector's size after the add,
that is `get_entity`'s job), addedCount (same number), landed[] (the bridge's positional
per-wire-entry `{index, landed}`), unconfirmed[] (the wire positions of entries the vector did not
confirm, review W2 #3), outcomeUnknown:true when any entry is unconfirmed (maybe-executed: read it
back with `get_entity`, never retry — a retry duplicates it), checked[], fgd, forced,
warnings, notes, unsavedBridgeEdits, saved:false}`; a reply without `added` is reported with the
fallback entries this server assumed (`index: null`, `count: null`, `addedFromRequest:true`, a
note) — never silently.

`remove` is deliberately thin: exactly one of `indices` (1..512 non-negative ints — the bridge's
own cap, review W2 #8; this tool's `connections` cap of 64 does not apply here) or `match` (at
least one of `output`/`target`/`input`) is sent on, and the **bridge** resolves it against the live
vector inside the write's dispatch — duplicate/out-of-range indices and a zero-hit `match` are
refused there with nothing opened. The zero-hit `-32026` (m2.18 `ConnectionNotFound`, live) is reported as `ConnectionNotFound` (the
node exists, the connection does not; retry-safe), every other refusal passes through. `removed`
comes back ascending, `removedCount` is the bridge's `count` (the number REMOVED — review W2 #1:
reading that as the remainder under-reported what the entity holds, which is what the tool used to
do) and `remaining` is the bridge's explicit post-removal vector size (`null` + a note on a bridge
that does not send it). The notes say that indices are
positional (`expectDoc` catches a document switch, not an edit that shifted the vector — prefer
`match`). Both tools count the edit on success
(`note_unverified_edit`), like `set_entity_keyvalues`.

The live read: bridge m2.18 `entity.get` reports the vector as `connections` (empty array for a
node without any) with the `.vmap` reader's names — `outputName`, `targetType` (7 for what the
bridge writes), `targetName`, `inputName`, `overrideParam`, `delay`, `timesToFire` — and
`get_entity` (`source:"live"`) passes it through (`entities_query.rs`; `source:"saved"` reads the SAVED
`connectionsData`).

Tests: `tests/entity_io.rs` (20) — the FGD output/input checks with base-chain inheritance, `force`,
the `prop_static` refusal, the keyword/unknown/shared targets, the FGD-less tree, the argument
matrices (including the 512-index cap and the 513th's refusal), the 64-entry `connections` cap, the
exact request JSON for both forms, the bridge's duplicate/out-of-range/zero-hit refusals, the
`readback:false` maybe-executed path (`unconfirmed` + `outcomeUnknown` + "do not retry", and the
confirmed negative control), the 5-minus-3 `remaining` regression (W2 #1), the two capability gates,
`undo_last` restoring the vector, the `connections` shape on `get_entity` and that `list_props`'
live keyvalues are unaffected; `crates/cs2-mcp-common/tests/mock_entities.rs`
(`connection_writes_shape_and_negatives`) pins the wire-level gates, the `added`/`count`/`landed`
shapes and the remove reply's `count`/`remaining` split. **Inferred (`[I]`)**: the match
comparison's case-insensitivity in the mock (the bridge's own is exact/case-sensitive — the Rust
tool does not normalize). Not live-verified as *tools* (the bridge methods are `[I: offline]` until
the orchestrator's live run; `test_props.py` is the live half and now asserts `landed`, the
`remaining` field and the origin text).
The add's body is one gate-free core, `connection_run` (`ConnectionSource::Node` reads the live
source and writes, `ConnectionSource::Class` is WP-T's pre-flight and sends nothing), so
`create_trigger` runs exactly the same FGD output/input checks against the class it is about to
create; `add_entity_connections` is a thin gate + envelope over it since WP-T.

## Triggers (WP-F/WP-L/WP-T/WP-V)

RE facts (the tie/untie/children recipes, the saved format, the material, compile + runtime):
[research/triggers-re.md](research/triggers-re.md).

A trigger is a **brush entity**: a `CMapEntity` whose FGD class is a `@SolidClass` with tied
`CMapMesh` children (the volume). The class resolving to a trigger (`TriggerOnce`/`Trigger`, or a
`tags( … Trigger … )` helper) makes it a *trigger* — `Database::is_trigger_class` requires the
Solid kind — but the mesh children are the brush-entity definition itself. The volume is normally
`materials/tools/toolstrigger.vmat` (every trigger class in `core/base.fgd` carries it as its
`metadata { auto_apply_material = … }`); Hammer itself never applies a material on tie, the tools
do (`re-triggers.md` Q4). The block is `hammer/triggers` (after `PROP_TOOL_ORDER`), five tools,
fixed order `TRIGGER_TOOL_ORDER` in `src/tools/triggers.rs`: `list_triggers`
(`src/tools/triggers_query.rs`), `create_trigger` / `tie_to_entity` / `move_to_world`
(`src/tools/triggers_live.rs`) and `set_trigger_volume` (`src/tools/trigger_volume.rs`), with the
volume/material argument parsing they share in `src/tools/trigger_args.rs` (review D1). The live
half is bridge m2.19: `entity.tieMeshes` / `entity.untie` / `entity.get` children. Plan:
`target/plan-triggers.md` §1 (the tool + m2.19 wire spec, §5 FINAL after `target/re-triggers.md`),
`target/WP.md`.

**The tools were accepted live end to end (2026-10-16, bridge m2.19 from the merged branch;
STATUS.md "FINAL LIVE")**: `create_trigger` (T/CT/push volumes with I/O, the refusals included) ->
`set_trigger_volume` resize -> `tie_to_entity`/`move_to_world` -> `list_triggers` (file + live,
with connections) -> `save_map`/`compile_map`/`run_map`, and the runtime verdicts: spawn in a
`trigger_once`/`trigger_multiple` box killed the kill props (`OnStartTouch -> Kill`), `trigger_push`
held the player at velocity z ≈ 600, `trigger_hurt` took hp 100 -> 90 -> 85 (`live/rt2.log`).

### FGD trigger and spawnflag lookup (`src/fgd`, WP-F, branch `triggers/wp-f`; FIX-V review items, branch `triggers/fix-v`)

No new tool and no bridge here: `fgd::Database` gained the lookups the trigger modules resolve
their classes and flags with (`src/fgd/mod.rs`), plus the fixture and tests for them.

- `is_trigger_class(class)`: the class is a **`@SolidClass`** (FIX-V adds the kind gate, so a
  Point class tagged `Trigger` and the `@BaseClass`es themselves are never triggers — only a
  Solid class is a brush volume) AND its resolved base chain (the same depth-first walk as
  `prop_tag`) carries a class named `TriggerOnce`/`Trigger`, or a `tags( … )` helper arg
  `Trigger` — case-insensitive. `[V fixture]` `trigger_once`/`trigger_multiple`/`trigger_push`
  are triggers; `func_buyzone` (Solid, base `Targetname` only), `func_wall_mini`, a Point class,
  an unknown class, and the `@BaseClass` `TriggerOnce`/`Trigger` (they carry the name/tag
  themselves) are not.
- `spawnflags_def(class) -> Option<&KeyDef>` (the resolved key, `None` for a class without one
  **and for one that declares `spawnflags` with a type other than `flags`** — `spawnflags(integer)`
  exists in Valve's files and is not a flag list; FIX-V),
  `default_spawnflags(class)` (OR of the `default_on` bits: `trigger_once` = `4097` = 1|4096),
  `decode_spawnflags(class, text) -> Result<SpawnflagsView{value, flags:[{bit,label,on}],
  unknown_bits}, SpawnflagsError>` (the text must be a non-negative integer after trimming; `""`
  is 0; undeclared bits come back as an `unknown_bits` bitmask) and
  `encode_spawnflags(class, &[Value]) -> Result<u64, UnknownSpawnflag>` (a declared label,
  case-insensitive exact, or an integer equal to a declared bit; the first bad value ends it).
  A class without `spawnflags` fails both directions (`NoSpawnflags` / `UnknownSpawnflag` with an
  empty `valid`), never a silent 0. `[V fixture + real tree]`
- `auto_apply_material(class) -> Option<String>`: the `metadata { … }` header helper's
  `auto_apply_material` value through the base chain (`TriggerOnce` ->
  `"materials/tools/toolstrigger.vmat"`); `None` when no class in the chain carries the key, so
  callers must not invent a material. The parser already kept the whole `{ … }` body in
  `helpers` (raw args), so `parser.rs` needed no change — pinned by
  `trigger_once_carries_the_real_flags_and_the_metadata_body`. `[V fixture + real tree]`
- **Behaviour change (WP-F; scope measured in FIX-V)**: a subclass that redeclares
  `spawnflags(flags)` **merges** its flags with the base class's in `resolve`/`upsert_key`
  (`merge_flag_defs`: a shared bit takes the child's label/default, a new bit is appended). Before
  this, the child's block replaced the base's, so `trigger_push`/`trigger_look` (which declare
  128/256 only) did not offer the 12 `TriggerOnce` flags. `[V real FGD: trigger_push resolves 14
  flags, 1 and 128 present]`; the merge matching Hammer 2's UI is `[I]` (not read out of a live
  Hammer; the plan requires it).
  `[V real FGD, FIX-V corpus `real_game_spawnflags_merge_scope` (`src/fgd/mod.rs` unit tests,
  `--ignored`)]` the scope is 14 of the 78 real classes that declare `spawnflags(flags)`: their
  resolved bit set is strictly larger than their own block (a plain override would have dropped
  the extra bits) — `trigger_push`/`trigger_look` (own 128/256 -> 14 bits), `func_door_rotating`
  (own 5 bits -> 14), `prop_dynamic_base` (own 5 -> 7), `BasePropPhysics` (own 7 -> 9),
  `func_physbox` (own 5 -> 12), `env_shooter` (own 2 -> 3), the six `phys_*constraint`/
  `phys_thruster`/`phys_ballsocket` (`[1,2]`/`[]`/`[32]` -> the base's 3-6 bits); 0 ambiguous.
  The merge therefore reaches `class()`/`get_entity_class`/`validate` for every one of them, not
  just the triggers. The remaining 64 redeclare a superset of their base (or have no base
  `spawnflags`), so they resolve identically either way.
- **FIX-V (review-rust.md B4) — the two gates on the lookups above**: `spawnflags_def` returns
  `None` unless the resolved key's type is `flags`, so a class declaring `spawnflags(integer)`
  (a few Valve classes do) answers `NoSpawnflags`/`UnknownSpawnflag` with an empty `valid` instead
  of decoding an empty flag list with `unknown_bits == value`; and `is_trigger_class` requires the
  class itself to be a `@SolidClass`, so the `@BaseClass`es `TriggerOnce`/`Trigger` (which carry
  the name/tag) and a `@PointClass` with `tags( Trigger )` are NOT triggers — only a Solid class
  is placed as a brush volume. Pinned by `spawnflags_def_requires_the_flags_type` and
  `is_trigger_class_requires_a_solid_class` on the new
  `fixtures/fgd/cases/spawnflags_type/` tree (and the two tests above were updated: the base
  classes moved to the negatives, `[V fixture + real tree]`).

Fixture: `fixtures/fgd/core/base_mini.fgd` appends `TriggerOnce` (the real 12 flags, the
`metadata { … }` block), `Trigger`, `trigger_once`, `trigger_multiple`, `trigger_push` (its own
128/256 block) and the non-trigger `func_wall_mini`. That moved the class counts in
`tests/entity_classes.rs` (16 classes: 4 point, 5 solid, 7 base), which is the only edit there.

Tests: `tests/fgd.rs` — `trigger_classes_are_recognized_by_base_chain_or_tag`,
`is_trigger_class_requires_a_solid_class` (FIX-V negative: a Point class with `tags( Trigger )`
and the `@BaseClass` `TriggerOnce` are not triggers),
`trigger_once_carries_the_real_flags_and_the_metadata_body`,
`subclass_spawnflags_merge_with_the_base_bits`,
`spawnflags_def_requires_the_flags_type` (FIX-V negative: `spawnflags(integer)` is not a flag
list),
`auto_apply_material_follows_the_metadata_helper_through_the_chain`,
`decode_spawnflags_reports_every_declared_flag`,
`encode_spawnflags_accepts_labels_and_declared_bits`; negatives
`decode_spawnflags_rejects_non_integer_text` (`"abc"`, `"1.5"`, `-1`, overflow),
`classes_without_spawnflags_decode_and_encode_are_errors` (no silent 0),
`encode_spawnflags_rejects_unknown_names_and_undeclared_bits` (unknown label, `3` for a 1/2 set,
`8192`, wrong-typed values, the first bad value wins), the two FIX-V tests running on the new
`fixtures/fgd/cases/spawnflags_type/` tree, and the `#[ignore]`d corpus
`real_game_trigger_helpers` (real tree: 12 flags, defaults 4097, `trigger_push` 14, the material,
`func_buyzone` not a trigger, `TriggerOnce`/`Trigger` not triggers either) plus the `#[ignore]`d
`real_game_spawnflags_merge_scope` in `src/fgd/mod.rs` (the 14-of-78 merge scope above). No bridge
method of its own; the lookups ride the live-verified trigger tools since the m2.19 live pass. Still
`[I]`: that the spawnflags merge matches Hammer 2's UI (not read out of a live Hammer).

### list_triggers (`src/tools/triggers_query.rs`, WP-L + FIX-C, branch `triggers/fix-c`)

ReadOnly, no declared bridge methods (like `list_props`: a file read never needs the bridge). Brush
entities of a map from the SAVED `.vmap` (route `file`, the entities-file plumbing:
`resolve_document`/`stale_read`/`source_json`, same path rules and error codes) or the live document
(route `live`: ONE `entity.find` at the bridge's 2000-row page + one `entity.get` per candidate,
bounded by the same 512 examine window `list_props` uses, `notes` naming every cut). Args:
`source`/`path` (the props route rule incl. `source:"live"`+`path` = `InvalidArguments`, non-string
`path` refused), `kind` `trigger|solid` (default `trigger`: `@SolidClass` classes whose resolved
chain carries a `Trigger`/`TriggerOnce` base or a `tags(Trigger)` helper -- now
**`Database::is_trigger_class`** (WP-F's merged API, FIX-C C2's switch-over; the Solid-kind gate
stays here, review B4), `Database::spawnflags_def`/`decode_spawnflags` behind the module's thin
`raw`-keeping wrapper), `class`/`targetname` (exact case-insensitive, empty refused; live forwards
both to the bridge), `within` {min,max} + **`withinMode`** `"contains"` (default) | `"intersects"`
(review C5: contains = the volume bounds lie entirely inside the box, intersects = an
edge-straddling trigger matches; a volume-less candidate is tested on its ORIGIN in both modes,
with a note when that applied) -- the bridge's `within` is a sphere; this is computed locally on
both routes AFTER the reads, because the volume decides. `includeKeyValues` (false),
`includeConnections` (true; false omits the field), `limit` 1..=2000 (default 200), `offset`. The
FGD is required on both routes (`FgdUnavailable`, never an empty success) -- the load mapping is
now the shared `live_common::fgd_or_error` (review C6; also `Volume::to_json` rounds through the
shared `vmap::mesh::round3`).

Per-trigger record `{nodeID, classname, targetname, origin, angles, meshes,
volume:{min,max,size,center,geometrySource?}, spawnflags:{value,raw,flags:[{bit,label,on}],unknownBits},
connections, problems, keyvalues?}` + the props envelope (`total/returned/offset/truncated`,
`summary.byClass`). **`connections` (FIX-C C1)**: the live route echoes the reply's `connections`
array (a3.log wire shape); the file route reads the entity's own `connectionsData` (the records
`get_entity` reports) -- it was always `[]` before the fix. Fixture: `fixtures/vmap/probe_conn.vmap`
= `probe.vmap` + one synthesized `DmeConnectionData` on the tested buyzone (nodeID 67,
OnStartTouch -> ta_kill_t Kill), byte-reproducible via the committed generator
`fixtures/vmap/patch_connections.py` (same pattern as `patch_scales.py`; verified against the
Python oracle read of the installed `cs_script_demo.vmap`). Real-tree route: the `#[ignore]`d
`real_tree_cs_script_demo_trigger_lists_its_connection` (kind trigger + the real FGD +
`OnTrigger scriptent RunScriptInput "GrenadeHit"`); the fixture test
`file_connections_come_from_connections_data` covers it without the game tree.

Volumes: union of the mesh children's bounds (`vmap` mesh records on the file route; the live
`mesh.get` read or the SAVED file on the live route, `geometrySource` = `live`/`saved-file`/`mixed`).
The saved file is a live-volume source **only while the document is clean** — `dirty:false`
REPORTED (`dirty_state(doc) != Some(false)`, review C3's rule: an unreported dirty state is NOT a
clean one, refused with a note), else `volume` null + note. Live `childrenTruncated:true` (review
C4): the listed meshes stay, `volume` is refused with `problems:["children list truncated by the
bridge"]` — a partial union would read as a shrunken box. No `children` member at all (an m2.18
bridge, modelled with a responder in the tests): `meshes`/`volume` null (never invented empty
lists) + one note; the plain mock carries the m2.19 `children` now (WP-T), so childless entities
answer `meshes: []` + `problems:["no mesh children"]` on the live route, the same rule the file
route applies.

Verified by `tests/triggers_query.rs` (fixture `.vmap` generated from the committed `probe.vmap`
by same-length patches with pinned occurrence counts: `func_buyzone`->`trigger_push` (4, the
in-test trigger), `func_bomb_target`->`unknown_bombzone` (3, the unknown-class case), and the
DMX **string-table** entry `parentname`->`spawnflags` (1 -- attribute names are table-shared, so
one byte renames the keyvalue on every entity that carries it; the empty text decodes to value 0)
plus a fixture FGD carrying the Trigger/TriggerOnce bases and every other probe classname as a
`@PointClass`) and over the mock bridge for the live route. Verified numbers: unpatched probe.vmapkind solid = 6 (33/34 func_bomb_target meshes 31/32, 70/67 func_buyzone meshes 71/66,
74 post_processing_volume mesh 75, 78 end_of_match with `problems:["no mesh children"]`), volumes =
the union of the mesh bounds (mesh 66: [-128,-384,0]..[320,-256,64], center = the entity origin);
default kind = 0 triggers. Negatives pinned: limit 0/"5"/2001, offset<0/1.5, `kind:"any"`/3,
bad `source`, non-bool flags, empty filters, within shapes + unknown key inside `within` +
min>max, unknown top-level arg, `source:"live"`+`path`, `path:123` -- all `InvalidArguments` before
the FGD load (then `FgdUnavailable` names the path, never an empty list), `FileNotFound`,
`NoDocument`, `BridgeNotRunning`, live `entity.find` error passthrough (`DocumentChanged`), live
`entity.get` failure as `problems:["live read failed: ..."]`, spawnflags text `"x"` ->
`problems:["spawnflags not an integer"]` (raw kept, value null; `""`/numbers parse), unknown bits
named in `unknownBits`. Live children-present (the mock carries the m2.19 `children`; the
children-ABSENT m2.18 shape is a mock **responder**, FIX-C): meshes listed, volume from the saved
file at the document's mapped path (`geometrySource:"saved-file"`) **only while the document
reports `dirty:false`** (a dirty/unsaved/unreported doc refuses it with a note), from the live
`mesh.get` read (`geometrySource:"live"`, exercised through a real `create_block` write), or
null + note naming the unplaceable mesh ids; `childrenTruncated:true` -> meshes listed, volume
refused with `problems:["children list truncated by the bridge"]`; no `children` member at all ->
`meshes`/`volume` null (never invented empty lists) + one note. An unknown class is confirmed only
by its own mesh children (listed under kind solid with `problems:["unknown class"]`, dropped
otherwise with a note -- the `list_props` unknown rule, mirrored). `#[ignore]`d corpus tests on the
REAL tree: `template_defuse.vmap` (real FGD; kind solid lists func_bomb_target 33/34 meshes 31/32,
func_buyzone 67/70 66/71, post_processing_volume 74/75, each with a non-null volume) and
`cs_script_demo.vmap` (kind trigger lists `trigger.grenade` 296 with its `OnTrigger -> scriptent
RunScriptInput "GrenadeHit"` connection) -- run with
`cargo test -p hammer-mcp --test triggers_query -- --ignored`; both were run green in this branch.

**[V live]** (2026-10-16, `live/a3.log` + the FINAL LIVE `live/b1.log`): the file and live routes
list the acceptance map's three triggers plus the FGD-trigger `post_processing_volume` with the
correct volumes (the resized push box top z 192 ✓), the `within` filter works, and the connections
are listed on both routes (the live route's reply echo and the file route's `connectionsData` —
FIX-C C1, verified live). Still mock-responder-only: the truncated-children volume refusal, the
m2.18 absent-`children` shape and the session-cache volume source (the acceptance flow read the
saved-file and live routes only).

The wire shapes the mock/fixture tests restate were pinned by the a3.log live evidence; the
underlying m2.15 reads are verified live via `list_props`. `src/vmap::Document::children_of` returns `Option<&[(String, Option<i64>)]>` (the
WP sketch said `Option<Vec<..>>` -- a reference to the cached doc's own memory, no clone; tests in
`tests/vmap_dmx.rs` pin element order and the `Some(&[])`-for-an-empty-array case). Shared-file
note: `tests/server.rs` and `tests/stdio.rs` pin the tool order end to end — `SOUND_TOOL_ORDER`
follows `TRIGGER_TOOL_ORDER` exactly (see the Sounds block).

### create_trigger / tie_to_entity / move_to_world (`src/tools/triggers_live.rs`, WP-T + WP-T2 + FIX-A, branch `triggers/fix-a`)

**[V live]** (2026-10-16, bridge m2.19; `live/a0`–`a2.log` acceptance + `live/b1.log` final live,
plus the mock `tests/triggers_live.rs`): the acceptance flow created `trigger_once`/
`trigger_multiple`/`trigger_push` volumes with I/O (the `UnknownSpawnflag`/`NotASolidClass`/
`UnknownOutput`/`MeshAlreadyTied` refusals included), tied into an existing entity (`entityID`) and
by class, the all-`alreadyTied` tie answered `noop:true` / `undoGroups:0`, a retie was refused
without and allowed with `allowRetie` (the emptied old entity deleted, one undo restoring both),
`move_to_world` was reverted by `undo_last {acceptUserEdits:true}` (Hammer's own group label), the
map compiled (`live/b2.log`) and the triggers fired at runtime (`live/rt2.log`: spawn inside the
boxes killed the kill props, `trigger_push` held the player at velocity z ≈ 600, `trigger_hurt`
took hp 100 -> 90 -> 85). Still `[I]` (never exercised live): a tie whose old parent is **locked**
(`NodeIsLocked`), untie of an entity inside a **group/prefab** (the children go to the entity's
parent — `[V bin]` only, no prefab in the scratch map), and a tie of a **rotated first mesh**
(angles copied to the entity, untested with non-zero angles).

**FIX-A (the review fixes of `target/review-rust.md` group A)** — what changed on this branch, on
top of the WP-T2 text below:

* **A1 (HIGH)**: the volume step (a) goes through `mesh_live::send_geometry` now, the same path
  `create_mesh` uses: the local `check_geometry`/`round4`/`as_float` copies are gone, and the
  request **byte budget** (`MAX_REQUEST_BYTES`) and the **M1 face-count check** apply. An oversized
  `geometry` volume is refused by our own budget instead of coming back as the bridge's `-32600
  "line too long"` (which closes the connection); when the bridge reports fewer `faces` than were
  sent, the result carries the mismatch note and no geometry is cached. Tests:
  `an_oversized_or_degenerate_volume_is_refused_before_the_mesh_is_sent`,
  `a_face_count_mismatch_is_reported_and_leaves_no_cached_geometry`. The volume's own
  `undoLabel` (A6) is the bridge's (`progress.push(&out, …)`, not a hard-coded string) —
  `the_volume_step_reports_the_bridges_own_undo_label`.
* **A2**: an explicit `spawnflags` list stays the **COMPLETE** set (no silent OR of the FGD
  defaults), but every `default_on` flag the list leaves OFF is now a `warnings` entry naming it
  and its bit (`spawnflags_plan`); both schemas say "the COMPLETE set … omit for the FGD defaults".
  `an_explicit_spawnflags_list_is_sent_verbatim` asserts the warnings (and that `4097` has none).
* **A3/A7**: `tie_refusal` takes the `TieTarget` and the bridge's `data.nodeID`: a named mesh is
  named in the message/data (`NotAMesh`/`MeshAlreadyTied`/`NodeNotFound`), a `-32022` without a
  node names the `entityID` (`data.entityID`) and only a class-form tie falls back to "one of the
  meshes (…)". Every bridge-refusal message says "the bridge refused it before opening an undo
  group: the document is unchanged" instead of the false "nothing was sent" (only local gates say
  that), and `NotASolidClass` spells the FGD keyword capitalised (`@PointClass`/`@BaseClass`).
* **A4**: the tie's `keyValues` echo is read (`keyValues_echo`, `unapplied_keyvalue_warnings`): the
  result carries `applied` (`{key: bool}`) and `keyValuesEcho`, every key that is not `applied:true`
  becomes a warning (with the bridge's own warning text), and a reply that carried **no** echo while
  this call sent keys gets the "not verified" note. `a_refused_keyvalue_is_warned_about_and_an_absent_echo_is_reported`.
* **A5**: `Progress` keeps the pre-call `DocState`, and `partial_failure` reads `doc.active` after a
  maybe-executed failure: the revert instruction then names the document's real undo-depth delta
  (`data.undoGroups`) with the label count as `data.undoGroupsMinimum` and the "read
  session_status" advice when the two disagree. A bridge refusal that names an opened group
  (`data.undoGroupOpened`, the in-group solid-class path) is counted as one group even when nothing
  of the call landed. `a_maybe_executed_tie_error_is_outcome_unknown` (now `undoGroups: 2` for the
  mock's applied tie) and the unit tests in the module.
* **A8**: `read_tie` keeps `label: Option<String>` plus `noop`: an all-`alreadyTied` tie (the
  bridge's `undoLabel:null`/`noop:true`, no group — FIX-B) is a success **no-op**: `undoGroups:0`,
  no unsaved edit, no cache invalidation, the "nothing to do" note.
  `an_all_already_tied_tie_is_a_noop_with_no_undo_group`.
* **A10**: the previously untested failure steps are covered —
  `a_connections_failure_names_the_mesh_and_the_entity` (step (c), `undoGroups:2`),
  `a_retexture_failure_names_the_entity_it_created` (step (b2)); the partial-failure test seeds the
  BRIDGE's `{reason, nodeID}` shape.
* **L13**: `move_to_world`'s docs/notes say the meshes go back under the entity's **PARENT** (the
  world, or the group/prefab the entity sits in), not "under the world".
* **A9 (mock, `crates/cs2-mcp-common/src/mock/hammer.rs`)**: the tie refuses with the bridge's
  `tie_err_node` shape (`{reason, nodeID}` for a mesh; no `nodeID` for a missing `entityID`), the
  **lock is checked before** the already-tied skip, the gdType refusal carries
  `undoGroupOpened:true`, an all-`alreadyTied` tie answers `{noop:true, undoLabel:null}` with no
  group/epoch move, and the reply carries the `handle` and the `keyValues {applied, warnings}` echo.
  `-32006 UnknownClass` stays (FIX-B aligns the bridge to it). `mock_entities.rs` pins all of it
  (`an_all_already_tied_tie_opens_no_undo_group`, the gate/echo assertions). The mock's untie list
  order and `childrenTruncated`-always-present shape stay as they are (the tool sorts; the key is
  asserted by other suites) and are noted in the mock.

`create_trigger {class? = trigger_multiple, exactly one of box {min,max} | geometry
{vertices,faces} | meshNodeIDs [1..64], material?, retexture?, targetname?, spawnflags?,
keyValues?, connections? [1..64], force?, allowRetie?}` (Mutating). Everything local runs first —
the class must be an FGD `@SolidClass` (`UnknownClass` + the database's own suggestions, else
`NotASolidClass {class, kind}`), the spawnflags resolve against the class's FGD flags (`spawnflags`
is the complete set, see A2 above), `keyValues`
are FGD-checked (issues are warnings, the `create_entities` policy) and
`classname`/`spawnflags`/`targetname`/`origin`/`angles`/`scales` inside `keyValues` are refused (the
fields above are their spellings), the volume's geometry is validated by `geometry::validate` for
the argument shape and then by `mesh_live::send_geometry` (rounding, the bridge's coordinate gate,
the face gates on the rounded numbers *and* on the bridge's f32 widening, the request byte budget —
A1; the local copies of those gates are gone), and the `connections` are checked through
`entity_io::connection_run`'s `ConnectionSource::Class` form — the class stands in for the source
entity, so an output the class does not declare (or a `prop_static` target) refuses the call **with
nothing sent, not even the volume**. Then the writes, under one write gate: (a) `mesh.create` of the
volume (`box`/`geometry` forms; the material is the argument, else the class's FGD
`auto_apply_material` — the real `core/base.fgd` value — else `materials/tools/toolstrigger.vmat`
for a class whose `is_trigger_class` is true, else the `create_mesh` default
`materials/dev/reflectivity_30.vmat` for a non-trigger solid class; `materialSource` says which),
(b) `entity.tieMeshes {class, nodeIDs, keyValues, allowRetie?, expectDoc}` (spawnflags and
targetname go as text; **omitted spawnflags send the FGD defaults explicitly** — `trigger_once`:
`"4097"` = Clients|Correctly-account-for-object-mass — and the note says so, so the compile does not
depend on the node factory applying them), (b2) `mesh.setMaterial` per named mesh for
`retexture:true`, (c) `entity.addConnections` on the new node. **Every step carries its own fresh
`expectDoc`** (a write moves the document's fingerprint) and every step's reply is read through
`live_common::post_write_document`: each step's post-write state is the `doc` of its own reply (nothing is cached, so the
steps need no extra bookkeeping). The result reports `nodeID`, `classname`, `targetname`, the bridge's `origin`
(a NEW entity's origin is the **FIRST mesh's**, Hammer's own rule — `[V live]` re-triggers Q1),
`meshes [{nodeID, created}]`, `entityCreated`, `alreadyTied`, `removedEmptyEntities`,
`material`/`materialSource`, the decoded `spawnflags` (`{value, raw, flags [{bit,label,on}],
unknownBits}` — WP-F's `unknown_bits` **bitmask**), `applied`/`keyValuesEcho` (A4),
`connections {count, added, unconfirmed, outcomeUnknown?}`, `undoGroups`/`undoLabels` (one Hammer
group per step; the notes say ``undo_last <n> times`` reverts), `unsavedBridgeEdits`, `saved:false`,
`fgd`, `warnings`, `notes`.
A failure **after step (a)** is `PartialFailure` (`-32012`): it names the created mesh nodeID, is
`retrySafe:false` (a retry would create a second volume), carries `data.step` / `data.bridgeError` /
`undoGroups` (+ `data.undoGroupsMinimum` when the count could not be pinned — A5); a `-32603`-style maybe-executed tie additionally sets `outcomeUnknown`.

`tie_to_entity {class? | entityID?, meshNodeIDs, targetname?, spawnflags?, keyValues?, material?,
allowRetie?}` (Mutating) is the bare tie primitive. **Exactly one of** `class` (create a new brush
entity; unknown/non-solid refused locally) and `entityID` (tie into an existing live brush entity,
§5); with `entityID` the `class`/`targetname`/`spawnflags`/`keyValues` arguments are
`InvalidArguments` and no FGD is needed (the FGD is loaded best-effort for the envelope only).
`material` given = a retexture of every tied mesh, one `mesh.setMaterial` group each. WP-T2/§5: a
mesh already under the target entity is skipped and echoed as `alreadyTied`, and an **all**-
`alreadyTied` call is a success no-op (`undoGroups:0`, no marker of an edit — A8); a mesh under
**another** brush entity is `MeshAlreadyTied {nodeID, parentNodeID}` unless `allowRetie:true`, which
moves it and deletes the emptied old entity in the tie's own group (`removedEmptyEntities`, one undo
restores both). `move_to_world {nodeID, deleteEntity?}` is `entity.untie` (one group;
`deleteEntity:false` is refused: Hammer's own move-to-world removes the emptied entity). Its label
is Hammer's own
**`"Untie from entity"`**, not an `MCP: …` label, so reverting it is
`undo_last {acceptUserEdits:true}` (the plain `undo_last` answers `UndoGroupNotOurs`). Bridge
refusals map to `NodeNotFound` (`data.nodeID` names the mesh, or the `entityID` when the bridge
named none — A3), `NotAMesh {nodeID}`, `MeshAlreadyTied {nodeID, parentNodeID}`,
`NotABrushEntity` (tie's `entityID` and untie both) and `NotASolidClass` (the bridge's in-group
gdType refusal, counted with the group it left behind); the unknown-output/input checks keep
`entity_io`'s `UnknownOutput`/`UnknownInput`, `force` skips them.

**Capability gating (WP-T2)**: the three tools declare only their m2.19 marker
(`entity.tieMeshes`/`entity.untie`), so an m2.18 bridge answers `NotImplemented` (`data.missing`)
**before anything is sent**. The *optional* methods are checked **per call**
(`require_call_caps` → `require_caps`), always before the first send: `mesh.create` only for a
`box`/`geometry` volume, `mesh.setMaterial` only for `retexture` on existing meshes (or a
`tie_to_entity` `material`), and `entity.find`/`entity.get`/`entity.addConnections` only when
`connections` is given — a bridge missing one of them still serves the calls that never use it.
`FgdUnavailable` (not a guess) when the FGD is missing and the call needs a class — `move_to_world`
needs none and `tie_to_entity`'s `entityID` form needs none either. **WP-T2 also dropped the WP-F
shim**: `spawnflags_def`/`default_spawnflags`/`encode_spawnflags`/`decode_spawnflags`/
`auto_apply_material` are now `crate::fgd::Database` calls (plan §2; WP-F merged), with
`fgd::UnknownSpawnflag` mapped to the same `UnknownSpawnflag` ToolError.

**Verified over the mock** (`tests/triggers_live.rs`, 30 tests, its own FGD fixture appending
`core/base.fgd`'s `TriggerOnce` -> `Trigger` chain): the exact `mesh.create`/`entity.tieMeshes`/
`entity.addConnections`/`mesh.setMaterial` request JSON (params, `expectDoc`, the keyvalue text), the
FGD material default vs an explicit one vs the `create_mesh` default for a non-trigger solid, the
spawnflags defaults/explicit lists (with the A2 warnings), the volume matrix, the volume's own
budget/face-count gates and the bridge's `undoLabel` (A1/A6), the tie's `keyValues` echo and its
absent-echo note (A4), the all-`alreadyTied` no-op (A8), the retexture path, the cache surviving the
tie (and not the connections step), `undo_last` x3 reverting the block, `tie_to_entity`/
`move_to_world` end to end, and the negatives: unknown / Point / Base class, every volume argument
(min>=max, non-finite, >1048576, degenerate face, empty/65/duplicate/non-int ids), the refused
`keyValues` fields, unknown spawnflags (label and undeclared bit; a class without `spawnflags`), a
connection output the class lacks (**nothing dispatched at all**), the bridge's
`-32022`/`not a mesh`/`already tied`/`not a brush entity` refusals (each naming the node — A3),
`class`+`entityID` both / neither
and `entityID` with the class-form arguments (nothing sent), the retie without `allowRetie`
(`MeshAlreadyTied`, request pinned) vs with it (`removedEmptyEntities`, one undo restores both), the
per-call capability gate (a reduced bridge serves a box-only/plain-tie call and refuses the
connections/retexture one before the first send), the partial failure with the cache dropped (each of
the tunable steps: the tie, the connections, the retexture — A10), the
maybe-executed `-32603` (with the document's own undo depth — A5), `deleteEntity:false`, the missing
FGD and the old-bridge gate;
`crates/cs2-mcp-common/tests/mock_entities.rs` (`tie_meshes_creates_the_entity_reparents_and_undoes`,
`tie_meshes_gates_leave_nothing_behind`, `tie_meshes_into_an_existing_entity_and_allow_retie`,
`tie_meshes_class_and_entity_id_gates_leave_nothing_behind`,
`an_all_already_tied_tie_opens_no_undo_group`,
`untie_returns_the_meshes_and_deletes_the_entity`, `entity_get_caps_the_children_list`) pins the
mock's m2.19 semantics (the `MockNode::parent` edge, first-mesh origin, `entity.get.children` cap of
1024, `alreadyTied`/`removedEmptyEntities`, the tie reply's `handle` and `keyValues` echo,
`-32022`/`not a mesh`/`already tied`/locked/`not a solid (brush) class` before the undo group opens —
each carrying `{reason, nodeID}` — `"MCP: tie meshes to entity"`
vs `"Untie from entity"`).

**Was `[I]`-coded against the wire spec, then accepted live**: the bridge half (m2.19
`entity.tieMeshes`/`entity.untie`/`entity.get.children`, WP-B) and whether `mesh.create` + tie
compile into a real trigger entity are both `[V live]` now (the compiled brush entities fired at
runtime — see the section head). The `materialSource` wording stays this tool's own contract (not a
wire field).

### set_trigger_volume (`src/tools/trigger_volume.rs`, WP-V, branch `triggers/wp-v`; FIX-V review items, branch `triggers/fix-v`)

`set_trigger_volume {nodeID, exactly one of box {min,max} | geometry {vertices,faces},
material?, meshNodeID?}` — **[V live]**: re-triggers.md Q2's live-proven path
(`mesh.setGeometry` on a TIED mesh takes WORLD coordinates, keeps its parent, re-centres the pivot
on the new bounds — the entity origin is not updated, Hammer's own behaviour, harmless) carried the
acceptance flow: the push trigger's box was resized to z 192 (`live/a1.log`) and the player was then
held at the top of the RESIZED box at runtime (`live/rt.log`, z ~184), and after `undo_last` a re-run took the FGD-default material (`live/b1.log`, the FIX-V fallback's flow).
Mutating, one undo group
(`"MCP: set mesh geometry"`), declares `entity.get` + `mesh.setGeometry` (both pre-m2.19, so no
capability gate; the write goes through `mesh_live::send_geometry`, `pub(crate)` since WP-V with
visibility only — same request limits, `expectDoc`, cache commit and maybe-executed taxonomy;
the result is `set_mesh_geometry`'s shape plus `entityNodeID` (the caller's id — the result's
`nodeID` is the MESH's) and `material`/`materialSource`).

Mesh selection from `entity.get` -> `children` (`[{nodeID, class}]`, only `class:"mesh"`
counts): none -> `NotABrushEntity` (also for the bridge's `-32602 "not an entity"` on a mesh/world
node; `-32022` passes through as `NodeNotFound`); exactly one -> it is the volume; several ->
`MultipleMeshes {meshes}` unless `meshNodeID` names one of them; a `meshNodeID` outside the
children is `MeshNotAChild` (WP.md named the negative, not the name). No readable `children`
in the reply (absent — an older bridge — or the spec's unreadable `null`) -> `BridgeTooOld`
(-32011, `blockedOn: "bridge m2.19 (entity.get children)"`), **unless** `meshNodeID` was given:
the m2.18 path then works (the plan's "m2.18 suffices ONLY IF the caller passes `meshNodeID`"),
with a note that the selector went out unvalidated and the bridge's own target gates answer.
`childrenTruncated` is folded into the `MultipleMeshes` wording/data; **FIX-V (review-rust.md
B2)**: for a TRUNCATED reply a `meshNodeID` that is not among the arrived rows is now sent anyway,
with a note (the list cannot prove it absent; the bridge caps it at 1024 rows) — refusing it
rejected a real child that fell off the list. Every refusal after the `entity.get` read says
"no write was sent (the document is unchanged)" — the read WAS sent (review-rust.md B3).

Material: `mesh.setGeometry` re-initialises every face with the base material it is sent, so the
tool must name one. The argument wins (`materialSource:"argument"`); without it the mesh's OWN
material is kept, resolved exactly like the rebuild operators resolve their geometry: the
live `mesh.get` read first (`"live"` — the acceptance flow's
create_trigger-then-resize sequence reads it), else the SAVED `.vmap` (`"file"`,
`vmap::mesh::mesh_by_id`; only its per-face strings are read, so the operators' scaled-node and
unproven-parent gates do not apply) — and the file route only while the live document is clean:
dirty / dirty-unknown / unsaved MCP edits are `-32016 UnsavedChanges` with the `save_map` hint
(the rebuild operators' gate, plan §1), a mesh the clean file does not hold is `MeshNotFound`,
an untitled document `MeshGeometryUnavailable`, an unmapped path `PathNotMapped`.
**FIX-V (review-rust.md B1)**: those four refusals — and a clean file whose mesh lists no
material of its own — stand only for a **non-trigger** class. For a class the `entity.get` reply
names as a trigger class (`fgd::Database::is_trigger_class`, which now requires a `@SolidClass`)
the tool falls back to the class's FGD `auto_apply_material` (else
`materials/tools/toolstrigger.vmat`) with `materialSource:"fgd-default (mesh material unknown)"`
and a note naming the refusal, so the `create_trigger` -> `undo_last` -> resize
sequence works without saving a half-undone map or spelling a material the FGD already knows; a
trigger's material is invisible in game, while the refusal stays where the material shows.
A class the FGD does not carry, a `@PointClass`/`@BaseClass`, and an unavailable FGD take no
fallback. A non-trigger clean file whose mesh lists no material still sends the trigger default as
`materialSource:"default (the saved mesh lists no material)"`. The rebuilt volume's faces all take
the one base material (the old per-face split does not map onto the new volume; the note says so).

Verified over the mock (`tests/trigger_volume.rs`, 20): the exact `mesh.setGeometry` request
JSON (world box on the child mesh, `expectDoc`, material), the result shape, the mock world after
the write (mesh stays tied, pivot re-centred, geometry replaced), the cache and file material
routes (the file route on the committed `probe.vmap` fixture), the explicit material on a dirty
document (no source read), `undo_last` restoring the previous volume, and the negatives of
target/WP.md: not a brush entity (point entity, mesh node, dead id), several meshes without
`meshNodeID`, `meshNodeID` not among the children, dirty / dirty-unknown / unsaved-edits ->
`UnsavedChanges` for a NON-trigger class (nothing sent), a mesh missing from the saved file, the
box/geometry matrix and unknown/ill-typed arguments (all `InvalidParams`, nothing sent),
`children` absent -> `BridgeTooOld` (and the same bridge passing with `meshNodeID`), the write's
`-32022` -> `MeshNotFound` and the maybe-executed `-32603` -> `outcomeUnknown`.
The FIX-V additions: `a_trigger_class_falls_back_to_the_fgd_material_when_the_document_is_dirty`,
`the_trigger_fallback_prefers_the_fgd_material_over_the_constant`,
`a_trigger_class_falls_back_when_the_saved_file_cannot_supply_the_material`,
`the_fgd_fallback_does_not_apply_to_non_trigger_classes` (NEGATIVE: func_buyzone / an unknown
class / a Point class keep `UnsavedChanges`) and
`a_truncated_children_list_sends_the_unchecked_selector` (each verified to fail with the fix
removed: `UnsavedChanges`, `MeshNotFound`, `MeshNotAChild`).
**Live beyond the acceptance flow, the mesh-selection corner cases stay mock-verified** (a
truncated children list, an m2.18 bridge without `children`, several meshes without a selector —
the mock responders; the acceptance flow never hit them). The FIX-V fallback answered the WP-V
caveat live: after an `undo_last` a re-run without an explicit
`material` on a trigger class took the FGD material instead of being refused (`live/b1.log`) —
pass `material` to pin a different one.

## Sounds

RE facts (the compile route, the runtime loading, the packed-params positions, the drain shape,
the Hammer quirks): [research/sounds-re.md](research/sounds-re.md). A **soundevent** (`.vsndevts`
KV3 text, compiled to `.vsndevts_c` resource containers) defines *what* a sound is; the **sound
entities** (`tags( Sound )` FGD classes: `snd_event_*`, `snd_soundscape`/`env_soundscape*`,
`ambient_generic`) play soundevents at points, in volumes and on I/O timing. The block is
`hammer/sounds` (after `TRIGGER_TOOL_ORDER`), five tools, fixed order `SOUND_TOOL_ORDER` in
`src/tools/sounds.rs`: `list_soundevents` (`src/tools/soundevents_query.rs`),
`edit_soundevents`/`compile_soundevents` (`src/tools/soundevents_edit.rs` + the raw-text splicer
`src/soundevents/text.rs`), `create_sound_entities` (`src/tools/sound_entities.rs`) and
`drain_sounds` (`src/tools/sound_runtime.rs`), with the sound-name check the generic entity tools
and `create_sound_entities` share in `src/tools/sound_check.rs` over the library `src/soundevents`
(`index.rs` = the cached name index, `text.rs` = the comment-preserving splice). The only bridge
method in the block is `sound.drain` (m2.9; m2.20 added the packed params the drain positions come
from) — `list_soundevents`/`edit_soundevents`/`compile_soundevents` touch no bridge at all, and
`create_sound_entities` rides `edit.batch`/`entity.create`, the pre-existing entity writes.

### The soundevent index (`src/soundevents/index.rs`)

**The index** is a library: one entry per event name, built from four sources in precedence order
(best first, an entry's `sources` lists every holder, exactly like `model_catalog`): `addon-game`
(`game/csgo_addons/<addon>/soundevents/**/*.vsndevts_c` — what the runtime loads), `addon-source`
(`content/csgo_addons/<addon>/soundevents/**/*.vsndevts`, KV3 text through `cs2_kv3::parse`),
`vpk:csgo` and `vpk:core` (`soundevents/**/*.vsndevts_c` in `game/csgo/pak01_dir.vpk` /
`game/core/pak01_dir.vpk`). A compiled block goes through `resource::Resource` +
`resource::decode_kv3`; the text source through `cs2-kv3`; both become the same `properties` JSON
object, so every reader below works on one shape. `SoundEventEntry` carries
`name, file, sources, compiled, type, base, volume, vsnds, children, position, properties` where
`vsnds` is `vsnd_files_track_01` + `vsnd_files` (`vsnd_files_track_*` in key order) and `children`
is `soundevent_01`, both string-or-array normalised; `position` is a 3-number array. A copy's
`file` is the VPK-internal path for a pak copy (`soundevents/ambience/game_sounds_dust2.vsndevts_c`;
`sources` names which pak) and the app-root-relative path for an addon copy
(`game/csgo_addons/<addon>/soundevents/soundevents_addon.vsndevts_c`). `get` is exact and
case-sensitive (the engine hashes names — assumption A1), `suggestions` puts the case-insensitive
exact match first and then names sharing a ≥3-character prefix by shared-prefix length,
`resolve_chain` returns the **base-first** chain (the named event last, so folding it left to
right lets the child override the base; `merged` is that fold) and cuts a name that reappears at
`cycle:true` (bounded at 16), and `known_property_keys()` is the union of the indexed events'
property keys — `edit_soundevents`' `unknownKeys` warning; **403 keys** in the real corpus, so the
corpus is the authority on what exists (the compiler accepts an invented key silently —
research/sounds-re.md Q1). `sound_files` (`sound_asset_known`) is the lowercased `sounds/**` asset
path set from both paks and both addon trees: the `list_soundevents` `name` route's `vsnds[].known`
verdict (a path as written or its compiled `_c` spelling; `null` for an absolute/`..` path that
cannot be looked up).

**Caching.** `index_for`/`check_sound_name` (`index_for_report` carries the `rebuilt` flag) build
once inside `tokio::task::spawn_blocking` and cache on `HammerState.soundevent_index`; `fresh()`
re-`stat`s the two game VPKs, the addon's two `soundevents` directories **and** every addon
soundevent file it read, so a rewrite, a recompile or a file appearing in the directory is noticed
on the next call. **Nothing built is thrown away**: `soundevent_index_invalidate` (the file
writers' hook) drops the slot but parks the index on `HammerState.soundevent_index_last`, and
`index_for_report` takes a stale slot (or the parked one) as the *previous* index: while both paks
are untouched (`SoundEventIndex::vpk_unchanged`, their recorded mtimes re-`stat`ed) the answer is
`rescan_addon` of it — an `edit_soundevents`/`compile_soundevents` costs one addon walk, never a
re-decode of the 193 corpus blocks. A moved pak (or the `Default` stub, which carries no VPK half)
still rebuilds everything, and the parked index is dropped as soon as a fresh one is in place (one
index per server). **The game half survives every rescan, not just the first**: `assemble` copies
it out of the shared `GamePart` (the VPK watches, the lowercased `sounds/**` asset set, the VPK
notes and the non-event root members) and never takes it, so `fresh()`/`vpk_unchanged()` keep
watching the paks after an edit — a second edit rescans too — the `list_soundevents {name:…}`
`vsnds[].known` verdict keeps answering `true` for VPK sounds after an edit, and no VPK note is
lost. The `GamePart` is behind an `Arc`, so a rescan copies the watches, the sound-path set and
the note strings only, not the ~21.8k decoded property objects. `rescan_addon` re-walks only the
addon: the decoded VPK half (copies, notes, the `sounds/` set and the VPK mtimes) is reused, so it
works even after the paks became unreadable (pinned by a fixture test that deletes the csgo pak
before the rescan; a second fixture test overwrites the pak with garbage *while restoring its
mtime*, so a full build could only fail `CatalogVpkError` — the rescan keeps every pak event).
Two VPKs are required: `VpkMissing {path,
detail}` for an absent one and `VpkUnreadable` (tool name `CatalogVpkError`) for an unparseable
one — never an empty list that reads as "no soundevents". The addon sides may legitimately be
absent: a missing addon-game/addon-source directory, a `.vsndevts_c` that is not a compiled
resource / has no `DATA` block / has a corrupt one, and a `.vsndevts` text that does not parse
(the note names **line and column**) are all `notes` entries, and the index keeps answering from
the sources that did read. Scalar root members that are not events
(`_auto_calc_vnsd_duration_`, in almost every real block) are reported **once per build**, never
once per file.

**The stale verdict.** `check_sound_name` used to answer `Known` for any name whose *some* copy is
compiled, which hides a rename: after `edit_soundevents rename mcp.a -> mcp.b` the stale
`soundevents_addon.vsndevts_c` still holds `mcp.a`, so a `create_sound_entities`/`set_*` of `mcp.a`
passes the check and the entity is silent as soon as the recompiled `_c` lands. The index now
records, per name, whether its **only** holder is the addon's `_c` (`sources == [addon-game]`)
while the `.vsndevts` file paired with that `_c` (the same relative path under
`content/csgo_addons/<addon>/soundevents/`) exists and carries no such name —
`SoundEventIndex::stale_source(entry)` names the source file, and `check_sound_name` answers
`Stale { file }` instead of `Known`. A name a game pak also holds, a name the source still holds, a
name whose source file is gone, and every non-addon name stay `Known`; a `.vsndevts` that exists
but does **not** decode (a syntax error, non-UTF-8) is recorded as *unparsed* and is skipped by
the rule — its parse failure says nothing about what its `_c` holds, so none of that `_c`'s names
is reported stale (the parse note ends `(its names' staleness against the compiled file is
unknown)`; before this, one broken source made every name of its `_c` look stale). The real tree
reports **0 stale names** (the corpus test asserts it and prints the list — the seeded addon's `_c`
was compiled from the source that still carries every name). `Stale` is a **warning**, never a
refusal: the name may be the one the user is about to add back (`sound_check`'s wording: "removed
or renamed in the addon's soundevent source … run compile_soundevents and use the current name";
`create_sound_entities` mirrors a `SourceOnly` note plus that warning). `_c`-newer-than-source
mtime is deliberately **not** part of the rule: `edit_soundevents` moves the source's mtime on
every write, so an mtime rule would mark every just-edited name stale until a compile.

**Padded names.** `get` stays exact — `"mcp.snd.point "` is not the name the engine hashes — but
`suggestions` no longer treats a padded request as "the request itself": the trimmed exact match
is the first suggestion (`"Weapon_AK47.Single "` -> `Weapon_AK47.Single`), and the `sound_check`
refusal adds "the name has leading/trailing whitespace (the event names are '<trimmed>')".

**Measured on the real tree** (`#[ignore]`d corpus tests, 2026-10-16, debug build): 195 files
decode (the 193 corpus blocks = 183 csgo + 10 core, plus the mcp_test addon's `_c` and its
`.vsndevts`), **21,862 event names** at the first measurement — **21,863 after the live
acceptance added `mcp.snd.timer` to the seeded addon**, so the corpus test asserts `>= 21_862`
(the floor that catches a degenerate index) rather than an exact count: `edit_soundevents` writes
into that file (top-ranked: 21,541 `vpk:csgo`, 312 `vpk:core`, 10 `addon-game`; the probe's
21,601 is the csgo pak alone), and the `properties` JSON of the whole index is
**15,615,315 bytes ≈ 14.9 MiB** — under the 64 MiB line, so `properties` stays **eager** (no lazy
file+offset re-decode). **Assumption A2 closes here: `builtMs` ≈ 1000-1063 ms in a debug build**
(< the 5000 ms budget; the corpus test asserts it and prints the numbers — re-measured 1025/1035 ms
with the same 195 files / 21,863 events / 15,615,315 bytes on 2026-10-16 after the live
acceptance), i.e. well under a second in a release build; no warm-up at startup and one build per
server session. **Assumption
A4 is answered and the plan's guess was wrong**: the `amb.*.base` templates are in **neither game
VPK**, so they resolve only through the addon's own file. The plan's candidate, the core pak's
901-byte `soundevents/soundevents_addon.vsndevts_c`, carries **0 events** (`amb.base`,
`amb.looping.atXYZ.base` and `amb.soundscapeParent.base` all come back
`file = game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c`, `sources =
[addon-game, addon-source]`) — the seeded template text carries the base blocks verbatim.
`edit_soundevents` must therefore tell the user to paste the base block into their own file, and a
`base` that resolves nowhere in the addon has no game fallback. The addon's own event is
`compiled:true` from both sides: `mcp.snd.point` (`base = amb.looping.atXYZ.base`, `vsnds =
["sounds/ambient/dust2/cablestress_01.vsnd"]`).

### The FGD Sound tag + the sound-key check (create_entities / set_entity_keyvalues)

No new tool and no bridge: `src/fgd/mod.rs` carries three additive lookups plus the generic tag
helper they are built on (`src/tools/entity_classes.rs`'s `tag?` filter uses the same one):

- `has_tag(class, tag)`: the class's own `tags( … )` header helpers, then its resolved base chain
  (the same depth-first walk as `prop_tag`), case-insensitive, each comma-separated argument
  trimmed — `tag_from_args` is the per-helper form. A class the database does not carry is never
  tagged. (`prop_tag`/`is_trigger_class` keep their own kind/name gates: a prop tag is one of
  three values, a trigger must be a `@SolidClass`.)
- `is_sound_class(class)` = `has_tag(class, "Sound")`: the FGD marker of every class that plays
  a sound event — `ambient_generic`, `point_soundevent` and its `snd_event_*` subclasses,
  `env_soundscape` and its `snd_soundscape*` subclasses. The tag is the marker, **not** the key:
  `trigger_soundscape` carries a `soundscape` keyvalue but no `tags( Sound )` in Valve's file (its
  key is a legacy `.vsc` name), so it is not a sound class.
- `sound_keys(class) -> Vec<&KeyDef>`: the resolved keyvalues whose FGD type is `sound`, in
  declaration order — `point_soundevent.soundName` (inherited by every `snd_event_*`),
  `env_soundscape.soundevent` (inherited by `snd_soundscape`), `ambient_generic.message` (the
  legacy file-or-event form). `env_soundscape.soundscape` (type `string`) is deliberately not
  one; a class the database does not carry has none.
- `list_entity_classes` gained `tag?`: keep only classes `has_tag` accepts, applied before
  `total` is counted and composed with `filter`/`kind`/`limit`. A tag nothing carries is an
  **empty list with `total: 0`** (not an error — the question has an answer), a non-string or
  blank tag is `InvalidArguments` with nothing read.

`src/tools/sound_check.rs` is the adapter the generic mutators call —
`check_sound_keyvalues(state, &FgdSource, class, &keyvalues, &exempt, allow_unknown) ->
Result<Vec<String> /*warnings*/, ToolError>`:

- it selects the class's `sound_keys`, skips the caller's `exempt` keys (a `set_entity_keyvalues`
  revert: the submitted value equals the live one — writing the entity's own value back needs no
  lookup) and runs `soundevents::check_sound_name` on every other sound-typed value **before**
  anything is written;
- `Known` writes silently; `SourceOnly{file}` warns "not compiled yet — run compile_soundevents";
  `Stale{file}` (the addon's `_c` still carries the name while the `.vsndevts` source next to it
  removed or renamed it) warns "removed or renamed in the addon's soundevent source … run
  compile_soundevents and use the current name" and writes — the name may be the one the user is
  about to add back, so it is a warning, never a refusal; `FilePath` (a `/` or
  `.vsnd`/`.wav`/`.mp3` value: `ambient_generic.message`'s direct-file form) and `Empty` (`""`,
  which clears the event) warn and write; `Unknown` is the **refusal**
  `SoundEventNotFound {name, key, suggestions, sourcesChecked: [addon-game, addon-source,
  vpk:csgo, vpk:core]}` (`retrySafe`, no wire code, nothing dispatched), and an index that cannot
  be built is the refusal `VpkMissing` naming the reason — never a silent pass. A
  **whitespace-padded** name is `Unknown` with the trimmed exact match as the first suggestion and
  the refusal's message says so: "the name has leading/trailing whitespace (the event names are
  '<trimmed>')".
- **Why a refusal and not an FGD-style warning**: a misspelt event name is silent at runtime (the
  engine hashes it, finds nothing and plays nothing) and the resourcecompiler checks no
  references at all (research/sounds-re.md Q1), so it is the hardest sound bug to find — the
  props' model check's argument.
- `allow_unknown` (`create_entities`' `allowUnknownSoundEvent:true`, `set_entity_keyvalues`'
  `force:true`) turns both refusals into warnings and writes the value unchecked. With the FGD
  unavailable the check cannot know which keys are sound-typed: it does not run and says so
  ("fgd unavailable: the sound-event check did not run … the value(s) were NOT checked") on the
  **entity's** warnings — only when a key that carries a sound is actually present. Without the
  FGD the only evidence is the key name, and the check's list is **every** key name `core/base.fgd`
  types as `sound` (**38** distinct names, listed from the installed FGD rather than guessed;
  `grep -hE '^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*\(sound\)' game/core/base.fgd game/csgo/csgo.fgd`
  is the list and `csgo/csgo.fgd` contributes none): the event keys (`soundName`, `soundevent`,
  `message`) and the plain
  sound keys of the brush/logic classes (`noise1`/`noise2` of `func_door`/`func_rotating`,
  `startsound`/`stopsound`, `use_sound`/`locked_sound`/`unlocked_sound`,
  `startclosesound`/`closesound`, `messagesound`, `explosion_custom_sound`, …). A key Valve types
  `sound` on one class and something else on another (`message`) is on the list: without the FGD
  the check cannot tell the two apart, and a missed sound key costs a silent unchecked write while
  an extra one only adds a warning. The old rule that warned about *every* keyvalue is gone — an
  `info_target` with a `targetname` is not a sound edit — and the warning names the key(s) it
  could not type. `strict` does not change any of this (it is not an FGD type issue).
- **The verdict source lives on the state**: `HammerState::sound_check_source`
  (`Option<SoundCheckSource>` behind a mutex, `None` = `Index` = the real index) with
  `HammerState::set_sound_check_source` as the test entry point, alongside the
  `SoundCheckSource::{Index, Fixed, Unbuildable}` variants — `create_sound_entities` reads the
  SAME state seam (below; there is no second seam and no per-tool injection), and the tool tests
  pin `Known`/`SourceOnly`/`Stale`/`FilePath`/`Empty` and the index-not-built refusal through it
  without waiting for a game tree. The old `thread_local` `sound_check::set_source` is **gone**:
  it was correct only because the tests happened to run on one runtime thread, and a
  `spawn_blocking` anywhere in the check's path (or a `flavor = "multi_thread"` test) silently
  read the real index. `the_sound_check_seam_is_the_state_not_the_thread` is the
  `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` pin: the tool call runs in a
  `tokio::spawn` (a worker thread, never the test's), so with a thread-local it read `Index`,
  whose build on the fixture tree fails `VpkMissing` — verified failing, then passing.

`src/tools/entities_live.rs` (the two hook call sites only): `create_entities` gained
`allowUnknownSoundEvent?` (default `false`) and calls the check per entity after the FGD
keyvalue check, merging its warnings into that entity's `warnings` — a refusal happens while the
batch is still being built, so an unknown name sends **nothing** (not even `doc.active`).
`set_entity_keyvalues` calls it after its FGD check, before the props' model check, with `force`
as `allow_unknown` and the revert keys as `exempt`; the warnings join the tool's `warnings`, and
a refusal leaves `entity.setKeyValues` unsent.

### list_soundevents (`src/tools/soundevents_query.rs`)

ReadOnly, no bridge methods — pure filesystem. The **list** route filters and pages the index:
`query` (case-insensitive substring of the name, or a glob over the whole name when it carries
`*`/`?`), `prefix` (case-insensitive name prefix), `source` (`any`/`addon`/`game`), `type`
(exact), `file` (case-insensitive substring of the top-ranked copy's file), `hasPosition`,
`includeProperties` (default false), `limit` (1..=1000, default 100) and `offset`; the result is
`{total, returned, offset, truncated, events[], summary, index, notes}` with one compact record
per event (`name, type, base, volume, source, sources, compiled, file, vsndCount, childCount,
position`). `summary` covers **every** match, not the page: `bySource` counts an event once per
source it appears in and `byType` the top 20 `type` values (typless events under `"(none)"`, ties
alphabetical). `index` is `{files, events, builtMs, rebuilt}`. The **detail** route (`name`,
exact/case-sensitive) returns `{event, resolved:{chain, merged, cycle}, children[], vsnds[],
index, notes}` — the entry plus its `properties`, every child with `known` (is the child an
indexed event) and every vsnd with `known` — and ignores the list filters; `name` together with
`query` is `InvalidArguments` (one is the list, the other the detail), and an unknown `name` is
`SoundEventNotFound {name, suggestions[], sourcesChecked[]}` with the suggestion ranking above
(`Weapon_ak47.single` suggests `Weapon_AK47.Single`). Every other argument error (`limit`
0/1001, a negative `offset`, an empty filter string, `source:"nope"`, an unknown argument, a
non-object argument list) is `InvalidArguments` before the index is even read; a missing game VPK
is `VpkMissing` on both routes.

### edit_soundevents / compile_soundevents (`src/tools/soundevents_edit.rs` + `src/soundevents/text.rs`)

`edit_soundevents` edits the addon's `content/csgo_addons/<addon>/soundevents/*.vsndevts`
**without destroying the user's comments** — the addon template is one of the two shipped KV3
files that do not round-trip through `cs2_kv3` (the parser drops comments, crate docs
"Limitations"), so the edit never re-serialises the whole file. Instead
`crate::soundevents::text` splices the **raw text**: a one-pass depth-1 scanner (string-,
`"""`-raw-, comment- and blob-aware, UTF-8/byte-wise) finds each top-level event span
`"name" = { … }`, and every op replaces/cuts/renames exactly one span. Text outside the touched
spans is copied byte-for-byte, so comments, blank lines and the file's CRLF survive. A comment
**inside** a replaced event is unavoidably lost and reported in
`commentsDropped:[{name, lines}]` — never silently. The spliced text is re-parsed and must equal
the intended document root before the write ([`text::roots_equal`]: member names in order,
values deep-equal; root-level key **quoting** style is deliberately not compared, because a
rebuilt key may be re-emitted quoted); any mismatch is `InternalError` and **nothing is
written** — a scanner bug refuses, it never corrupts the file. The whole read-modify-write (and
`compile_soundevents`'s exec) runs under [`HammerState::soundevents_gate`] — **not** the
server-wide write gate: a compile execs `resourcecompiler.exe` for up to its timeout and touches
no bridge state, so holding the write gate would stall every bridge tool for that long
(`a_bridge_mutator_completes_while_a_compile_is_in_flight` pins it). Edits and compiles
serialize against each other on that gate: two concurrent edits of one file land one on top of
the other (the second re-reads the first's written bytes), and an edit cannot interleave with a
compile's `_c` swap.

Ops (`1..=64` per call, all validated before any write; one failure refuses the whole call):
`set {name, properties, merge?}` (`merge:true` shallow-merges into the existing event, default
`overwrite:false`, so a plain `set` on an existing name is `SoundEventExists`; a `set` of a new
name inserts it before the root's closing `}`, quoted, Valve's template style), `remove {name}`
(a referenced event — via `base` or `soundevent_01` — is refused `SoundEventInvalid "referenced
by …"` unless top-level `force:true`, which warns; a remove is otherwise a pure cut, references
elsewhere are NOT rewritten), `rename {name, to}` (rewrites the event's key token and **exactly**
the `base`/`soundevent_01` string tokens of the referencing events — the same set
`soundevents_edit::rewrite_references` moves at the document level — reporting
`referencesUpdated`). A token that merely holds the same text somewhere else (another property's
value, a comment) is left alone on BOTH sides, so the round trip simply agrees; if a splicer ever
rewrote more than that reference set, the round-trip check would catch the divergence as
`InternalError` with nothing written. A new name that is not a bare KV3 identifier (first byte
`[A-Za-z_]`, then `[A-Za-z0-9_.]`, e.g. `1up`) is emitted quoted. Names are
`[A-Za-z0-9_.]{1..=127}` (the engine hashes the exact text — case matters [I] A1),
`properties` convert through `cs2_kv3::from_json` (a non-convertible value is `InvalidArguments`
naming the key), `position` must be 3 finite numbers, and each edited event must end up with
`type` or a resolvable `base` — in the edited file or the soundevent index — else
`SoundEventBaseNotFound` (+ index suggestions). `createFile:true` writes the template's exact
header line ([`TEMPLATE_HEADER`], copied verbatim from
`fixtures/soundevents/soundevents_addon.vsndevts`) + `{}`; `dryRun` validates + splices but
writes nothing; writes go through `write_atomic_with_backup` (the backup path is in the result)
and call `HammerState::soundevent_index_invalidate` afterwards. The soundevent index is
built only after every op passed its argument checks and the `unknownKeys` candidates are looked
up once after the loop, so an unbuildable index never masks an op's `InvalidArguments`.

Result: `{path, hostPath, created, written, dryRun, backup, ops:[{op, name, status:
created|replaced|merged|removed|renamed}], events, commentsDropped, referencesUpdated, warnings,
notes}`. A non-default `.vsndevts` path under the addon is allowed with a note that only
`soundevents_addon.vsndevts` is loaded by the game (research/sounds-re.md Q1 [V live]: the
compiler happily compiles any name; the template comment is the rule).

**Error types** (all local, retry-safe, nothing dispatched on them): `SoundEventNotFound`,
`SoundEventExists`, `SoundEventInvalid`, `SoundEventBaseNotFound`, `SoundEventsFileNotFound`
(hint `createFile:true`), `SoundEventsParseError {path, line, col}` (a file without the kv3
header is line 1); `InvalidArguments` (bad names, `ops` bounds/shape, `..` in a path),
`WriteOutsideAddon`/`PathNotMapped` (the content-path layer), `InternalError` (splice failure or
round-trip mismatch — the never-corrupt guarantee).

**Seams:** `BaseResolver` is a closure set over (a) `base` resolution, (b) "is this event name
known" (the `unknownChildren` warning) and (c) the corpus's known property keys (the
`unknownKeys` warning — the compiler checks nothing but syntax [V live Q1], so unknown keys are
silent at runtime; the corpus is the authority, `known_property_keys()`). The tests inject a
fixed resolver (`EditSeams.resolver: ResolverSource::Fixed`); **production is
`ResolverSource::FromIndex`** — the cached index (`soundevents::index_for`) is built LAZILY, at
the first base/children/keys check, so the ORDER is: argument validation, path resolution
(`WriteOutsideAddon`/`..`), the file existence/`createFile` check and the KV3 parse BEFORE the
index — a local refusal is never masked by a `VpkMissing`, and a call refused before those checks
never builds the index. When the index cannot be built: `validate:true` (the default) refuses
the edit with that error (nothing written); `validate:false` proceeds with an
`indexUnavailable` warning that no reference check ran. An empty corpus (an index with 0 events,
the seeded-default case) skips the key check. The `unknownVsnds` warning has no seam yet (a vsnd
path-set needs the VPK walk). `EditSeams.splicer` is the whole splice step:
`tests/soundevents_edit.rs` injects a corrupted splice to pin the `InternalError`-and-no-write
behaviour. `events` in `compile_soundevents`'s result is read from the fresh `_c` directly
(`crate::resource` container + `DATA` decode) — behind the index would be the same fact, which
changes nothing for callers.

`compile_soundevents` (`path?`, `timeoutMs?` = the shared `compile::compile_timeout` bounds)
refuses a missing file (`SoundEventsFileNotFound`) and then runs
`compile::compile_resourcecompiler` **unchanged** — `resourcecompiler.exe -nop4 -game
/app/game/csgo -i <container path>` in any tools container (`cs2mcp.tool` label; the route
verified live in research/sounds-re.md Q1) [V live]. On top of the shared result it adds:
* `errors:[{line, col, message}]` parsed from every `RESOURCE COMPILE ERROR` line
  (`Z:\\app\\content\\…\\x.vsndevts(5,12): RESOURCE COMPILE ERROR: …` [V live]; the Windows path
  is stripped), beside the shared `failLines`/`fatal`;
* `events`: the count re-read from the fresh `<file>_c`, `null` + a note when the re-read fails;
  it counts only the object-valued root members (the `_auto_calc_vnsd_duration_` scalar sits in
  almost every real block and is not an event — the same rule the soundevent index applies, so a
  real addon file no longer reports N+1).
* `notes:["loaded at the next map load (run_map); a running engine keeps the previous compiled
  file until then"]` [V live] and an `events`-re-read note when that failed.
`ok:false` (a `[FAIL]`, ` ERROR: 0 compiled, 1 failed, …` summary, or no output mtime move) is a
**result**, never an error; `RuntimeUnavailable` only when no tools container runs. A success
also invalidates the soundevent index (a failed compile writes nothing and does not).

### create_sound_entities (`src/tools/sound_entities.rs`)

`create_sound_entities {entities[1..=32], strict?, allowUnknownSoundEvent?, undoLabel?}`
(**Mutating**, one write gate; declares `edit.batch` + `entity.create` — the connection methods
are gated **per call**, so a plain create works on a bridge without `entity.addConnections`).
Each entity takes exactly one of `kind` `point|sphere|alignedbox|orientedbox|soundscape|
soundscape_proxy` (→ `snd_event_point|snd_event_sphere|snd_event_alignedbox|
snd_event_orientedbox|env_soundscape|env_soundscape_proxy`) and `class` (any other FGD class
that carries `tags( Sound )` through its base chain — refused `NotASoundClass` otherwise,
`UnknownClass` + nearest when the FGD does not know it; `create_entities` stays the generic
fallback). The tool owns the key spellings so an agent cannot get them wrong:

* `soundEvent` (required for the `point_soundevent` family and the soundscape family unless
  `legacySoundscape` names the `.vsc` route; **optional** for every other `tags( Sound )`
  class) is written to the class's `sound`-typed FGD key — `soundName` for the
  point_soundevent family, `soundevent` for the soundscapes, whatever an other class declares
  (e.g. `ambient_generic`'s `message`) — and goes through the soundevent index BEFORE anything
  is sent: `Unknown` refuses the whole call `SoundEventNotFound` (+ suggestions,
  `sourcesChecked`; a whitespace-padded name says so and names the trimmed one) unless `allowUnknownSoundEvent:true` creates it with a warning and
  `verdict:"unchecked"` (a misspelt event name is silent at runtime — the model-check argument);
  `SourceOnly` → note "not compiled: compile_soundevents"; `Stale` (only the compiled addon file
  still holds it) → warning, `verdict:"stale"` + `file`; a file path → note, written verbatim;
  an index that cannot be built refuses with its reason. An other class that declares **no**
  sound-typed key in the FGD (`snd_event_box_helper`, `snd_event_param`,
  `env_soundscape_proxy`/`snd_soundscape_proxy`, `snd_stack_save`, `snd_opvar_set*`,
  `trigger_snd_sos_opvar`) **refuses** an explicit `soundEvent` (`InvalidArguments` /
  `InvalidParams`, `-32602`, nothing sent, `retrySafe`): the name would be written nowhere, no
  verdict would run and the caller's intent — play this event here — would silently play nothing.
  The message says `class '<class>' declares no sound-typed key in the FGD; drop soundEvent, or
  pass the real key via keyValues`, and `keyValues` stays open for a key the FGD does not declare.
  Omitting `soundEvent` on those classes is unchanged: created with no sound key. With no FGD at
  all there is no key list to read, so the same refusal covers every class outside the family
  lists below — `ambient_generic` included, whose real key is `message`: pass it through
  `keyValues`.
* **The entity family comes from the FGD, never from the name**: the class or its resolved base
  chain contains `env_soundscape` → soundscape family (`snd_soundscape`,
  `env_soundscape_triggerable`/`snd_soundscape_triggerable` too), contains `point_soundevent` →
  snd_event family; everything else that carries `tags( Sound )` is "other"
  (`snd_event_box_helper`, `snd_event_param`, and the proxy pair — their bases are
  Targetname/Parentname, so `env_soundscape_proxy`/`snd_soundscape_proxy` are deliberately NOT
  the soundscape family: they carry `MainSoundscapeName` and no soundevent key, so a `soundEvent`
  on them is refused like on any other class without a sound-typed key, and `legacySoundscape` is
  refused as off-family). Without an FGD the base chain cannot be walked, so the real FGD's class
  list stands in: `env_soundscape`, `snd_soundscape`, `env_soundscape_triggerable` and
  `snd_soundscape_triggerable` are the soundscape family (they write `soundevent` +
  `enablesoundevent "1"`), every other `snd_event_*` is the snd_event family — **except**
  `snd_event_box_helper` and `snd_event_param`, which share the prefix but declare no sound-typed
  key and are "other" (exactly the two a bare `snd_event_` prefix rule gets wrong, which is why
  the list is spelled out instead of derived). An FGD-free `env_soundscape_proxy` or
  `snd_event_param` with a `soundEvent` is therefore refused like the FGD-carrying case.
* The soundscape family gets `enablesoundevent:"1"` (+ `soundevent`) or, with
  `legacySoundscape`, `soundscape:<name>` + `enablesoundevent:"0"` (written unvalidated, noted —
  the legacy `.vsc` route is not indexed; refused on every non-soundscape class).
* `radius` — allowed when the class declares a `radius` key in the FGD: `snd_event_sphere` a
  finite number > 0; the soundscape family and `env_soundscape_proxy` an integer ≥ 1 or `-1`;
  written as text that keeps the caller's shape (`"256"`, `"2.5"`); refused when the FGD gives
  the class no radius key. `box {min,max}` — only for classes whose FGD declares
  `box_mins`/`box_maxs` (`snd_event_alignedbox`, `snd_event_orientedbox`,
  `snd_event_box_helper`), per-axis `min < max`, |c| ≤ 1048576; the two fixed box kinds send the
  FGD default `"-64 -64 -64"`/`"64 64 64"` explicitly when omitted, with a note (an other class'
  box keys get exactly the caller's corners). `positions[1..=8]` — soundscape family only,
  written to `position0..7`. `startOnSpawn`/`toLocalPlayer`/`stopOnNew` bools → `"1"/"0"`. All of
  those are refused inside `keyValues` ("use the field"), as is `classname` (via the shared
  `keyvalues_arg`).
* `schedule {startDelay?, stopDelay?, repeatEvery?, repeatInitialDelay?}` (needs `targetname`;
  **refused for any class whose FGD carries no StartSound input** — the soundscape family,
  `snd_event_box_helper`, `snd_event_param`; `stopDelay` must be > `startDelay`) builds helper
  entities in the SAME batch: `startDelay`/`stopDelay` → ONE `logic_auto`
  `<targetname>_auto` (`OnMapSpawn -> <targetname> StartSound (delay startDelay)` /
  `-> StopSound (delay stopDelay)`), `repeatEvery` (+ `repeatInitialDelay` → `InitialDelay`) →
  ONE `logic_timer` `<targetname>_timer` (`RefireTime` text, `UseRandomTime "0"`, `OnTimer ->
  <targetname> StartSound`). **The timer's FIRST fire lands at InitialDelay + RefireTime after
  load** [V live]: `repeatEvery 5, repeatInitialDelay 1` → first StartSound ~6 s after the map
  loads, then every 5 s — plan the drain/observation times from InitialDelay + RefireTime, not
  from InitialDelay alone. Helper targetnames that already exist in the live document
  (`entity.find`) refuse the call before anything is written.

Flow (`create_trigger`'s shape): everything local first — argument parse, the FGD checks (issues
are warnings, `strict` refuses errors), the sound verdict, the helper-clash reads, and the
connection pre-flights (`entity_io::connection_run` with `ConnectionSource::Class`, the entity's
class and each helper's class standing in for what the batch will create: an output the class
does not declare — `OnStartTouch` on a `snd_event_point` — refuses `UnknownOutput` with **nothing
sent**) — then ONE `edit.batch` of `entity.create` ops for the entities AND their helpers (one
undo group, `MCP: batch`; `expectDoc`; `note_unverified_edit`), then per entity that has
`connections`/`schedule` one `entity.addConnections` on the new nodeIDs (`ConnectionSource::Node`,
one undo group each — the targets only resolve after the batch). Result: `{count, requested,
entities:[{index, kind, class, nodeID, targetname, origin, soundEvent:{name, verdict, file?},
keyValuesSent, helpers:[{role, class, nodeID, targetname}], connections:{added, unconfirmed},
warnings, notes}], undoGroups, undoLabels, unsavedBridgeEdits, saved:false, fgd, warnings,
notes}` — `undo_last` N times reverts (the notes say so). A failure after the batch is
`PartialFailure` (`-32012`, `retrySafe:false`) naming every `createdNodeIDs` and the `undoGroups`
opened ("undo_last N, or delete_nodes [...]"); a maybe-executed batch (`-32603`…) keeps the
bridge error's own name with `outcomeUnknown:true` and the unsaved counter (`create_entities`'
convention).

The sound verdict reads the shared state seam: `sound_check::verdict(state, name)` —
`HammerState::sound_check_source`, the same `SoundCheckSource::{Index, Fixed, Unbuildable}`
`create_entities`/`set_entity_keyvalues` read (there is no second seam and no per-tool
injection). An unbuildable index (`VpkMissing`) refuses the call with its reason — never
silently unchecked.

**Mock note**: `cs2-mcp-common`'s `ENTITY_CLASSES` carries the eight sound/logic classes and
then the five relatives the FGD-derived-family work added (`snd_soundscape`,
`env_soundscape_triggerable`, `snd_soundscape_triggerable`, `snd_event_box_helper`,
`snd_event_param`) — the real bridge's `CMapNodeFactory` registry carries every FGD class and
research/sounds-re.md Q4 verified `entity.create` of exactly these live, but the mock's list
predated them.

**[V live]** (2026-10-16, the acceptance flow — see Live acceptance): the flow ran live —
`create_sound_entities` created the startOnSpawn point (its sound at the entity's origin), the
scheduled pair (`logic_auto` delay 3, `logic_timer` `RefireTime 5`) and the soundscape, whose
events drained with exact positions and timing; the control (a point without `startOnSpawn`)
stayed silent until its timer. Hammer applies every one of these keys verbatim, fills the FGD
defaults around them, and does not validate soundevent names; `box_mins`/`box_maxs` are
entity-local and are written verbatim. Still `[I]` (recorded by no observation in the
acceptance): whether Hammer draws the box/sphere helpers on these keys (the viewport render
check); the mock pins are the exact wire JSON, the helper flow and the `PartialFailure` shape.

Verified over the mock (`tests/sound_entities.rs`, 33 tests, its own FGD fixture appended to the
`fixtures/fgd` copy — the triggers precedent; `tests/common/mod.rs`'s `PROPS_FGD` untouched; the
fixture gained the real-FGD relatives `snd_soundscape`, `env_soundscape_triggerable`,
`snd_soundscape_triggerable`, `snd_event_box_helper`, `snd_event_param`, `snd_soundscape_proxy`,
`snd_stack_save`, `snd_opvar_set_point` and the real `env_soundscape_proxy` shape
(`MainSoundscapeName` + `radius(integer)`, no soundevent key), and the mock's `ENTITY_CLASSES`
the five classes the family work needed, additive): the exact `edit.batch` request JSON (all six
ops with the
keyvalue spellings: `soundName`, `startOnSpawn:"1"`, `radius:"256"` vs the soundscape `"512"`,
`box_mins`, `enablesoundevent:"1"` + `soundevent`, `position0..1`, the timer's
`RefireTime`/`InitialDelay`/`UseRandomTime "0"`) and the two `entity.addConnections` requests
(the `OnMapSpawn` delay and the `OnTimer`), the undoGroups/undoLabels arithmetic, `undo_last` ×
2 reverting entity + helper, the sourceOnly/filePath notes, the FGD-less tree warning, and the
negative list: kind+class both / neither, `info_target` → `NotASoundClass`, unknown class →
`UnknownClass` + suggestions, sphere radius 0/negative/non-number, box on a point / min ≥ max,
soundscape without soundEvent and without legacySoundscape, soundscape + schedule, schedule
without targetname, stopDelay ≤ startDelay, 9 positions, the owned spellings inside `keyValues`,
unknown soundevent → `SoundEventNotFound` (+ suggestions, nothing sent; `allowUnknownSoundEvent`
creates with `verdict:"unchecked"`), an output the class lacks → `UnknownOutput` (not even a read
sent), a helper targetname already in the document (the pinned `entity.find`), a maybe-executed
`-32603` batch (outcomeUnknown, cache dropped via a committed `create_block` entry),
`entity.addConnections` failing after the batch → `PartialFailure` naming the created nodeIDs,
and the old-bridge gate (`NotImplemented` before any send — `doc.active` included — with a plain
create still working; argument negatives incl. unknown top-level/entity/schedule members,
bounds 0/33, and `repeatInitialDelay` without `repeatEvery`). The FGD-derived family specifics
(pinned in the same suite): `snd_soundscape` and `env_soundscape_triggerable` are the soundscape
family through their base chain (both created with `enablesoundevent:"1"` + `soundevent`, no
`soundName`), the triggerable refuses a schedule, `snd_event_box_helper` takes a box with no
`soundEvent` (no family keys), `snd_event_param` is created without a sound event, a `soundEvent`
on a class whose FGD declares no sound-typed key is **refused** with nothing sent
(`snd_event_param`, `snd_event_box_helper`, `snd_opvar_set_point`, `snd_stack_save`,
`snd_soundscape_proxy` — not even `doc.active` is sent, and the same class without a `soundEvent`
still creates), the FGD-free family list (`snd_soundscape`, `env_soundscape_triggerable`,
`snd_soundscape_triggerable` write `soundevent` + `enablesoundevent "1"`; `snd_event_param` stays
"other", refusing a `soundEvent` and creating with no sound keys without one), schedule is refused
for every class whose FGD lacks a StartSound input (`snd_event_param`, `snd_event_box_helper`),
`legacySoundscape` is refused off the soundscape family, and the proxy kind creates with no
sound keys at all while its own `radius(integer)` is accepted.

### drain_sounds (`src/tools/sound_runtime.rs`)

`drain_sounds` reads **and consumes** the bridge's sound-event ring (`sound.drain`, m2.9; the
fifth sound tool, last in `SOUND_TOOL_ORDER`) and, since bridge m2.20, decodes the server's
packed sound params into a world `position`. **Mutating** (the ring is destroyed on read),
declares `sound.drain`, holds no write gate and reads no document — it is engine state, not
Hammer state.

**The ring.** The bridge wraps the sound-op system on first use (and `sound.status` installs it
too: `drain_sounds {installOnly:true}` sends exactly that method and nothing else) and records
every start that goes through `ISoundOpSystem` slots 11/12/13 into a 256-entry ring. The drain
hands over what is there and destroys it; the events that do not fit the bridge's 96 KB reply are
requeued (`truncated:true`) and overflow is counted in `dropped`. So the tool cannot be idempotent
and says so in its notes ("call again while truncated is true"). Args: `max` 1..=256 (default
256 — the bridge's own default is 128, so the value is **always** sent), `resolve` (default
true; false skips the entity read), `filter {namePrefix?, names? (1..=32, exact and
case-sensitive), within? {center:[x,y,z], radius>0}}`, `installOnly?`. Every arg list is closed
(`InvalidArguments`, nothing sent); `filter` + `installOnly` is refused rather than ignored, while
`max`/`resolve` with `installOnly` are ignored **with a note**. Result: `{events, matched,
unmatched, total, dropped, truncated, pending, notes}`. `total`/`dropped`/`pending`/`truncated`
are the bridge's own numbers passed through; `matched + unmatched` is the number of events this
drain returned (a filtered-out event was still **consumed**). A reply without an `events` array is
`InternalError` naming the keys it got — never an empty success. Capability gating is the
registry's (nothing sent, `NotImplemented {data.missing:["sound.drain"]}`), so `installOnly` needs
`sound.drain` advertised too: the bridge answers `sound.status`/`sound.drain` from one `op_sound`
handler, so the pair is never split in practice. `-32017 PrerequisiteFailed` (the soundsystem
pins) and every other bridge error pass through unchanged.

**Where a position comes from** (research/sounds-re.md Q3 [V live + V bin]). The bridge reports
the SOS slot, `entindex`/`entityName`, `guid`, `hash`, `name`, `flags`, `ageMs` (measured at the
drain) and, with `resolve:true`, `origin`. A `snd_event_*` started by `startOnSpawn` arrives as
`{via:11, entindex:0, entityName:"worldent", origin:[0,0,0]}` — the sound entities are
server-only, so the client-side wrap cannot name them and the **entity** origin is not the
event's location. Slot 11's argument is a `SosStartParams_t` whose `+0x08` is the server's
packed-params buffer and `+0x1C` its length; bridge m2.20 copies at most 128 of those bytes into
the ring entry as `params {len, hex, truncated}` (`null` when the pointer is unreadable, absent
for slots 12/13/81) and this tool walks them: entries are
`u32 hash LE | u8 type | u16 size LE | data[size]`, and a `snd_event_*` carries
`4dce7c5a 0a 0c00 <f32 x><f32 y><f32 z>` — hash `0x5A7CCE4D` (`position`, the
`CC_snd_sos_start_soundevent_at_pos` param), type `0x0a`, size 12, the entity's Hammer origin
exactly. Each event gains `position: [x,y,z] | null` and `params {len, bytes, truncated, decoded,
fields:[{hash:"0x5a7cce4d", type, size, name?, value?}]}` (a type-10/size-12 vector is the float
array, every other kind is `0x…` hex). `decoded` is true **only** when the walk ends exactly at
`len` and the copy was not truncated; a truncated copy, a walk overrun or a `len` the bytes do
not match keep the entries that could be read whole but report `decoded:false`, and `position`
stays `null` — a half-read buffer never invents a location.

**"Old bridge" vs "unreadable"**: an m2.20 bridge writes the `params` key on *every* via-11
event — `null` when the pointer was unreadable — so absence of the key on a via-11 event is the
pre-m2.20 bridge itself. On that shape the event keeps `params:null`/`position:null` and the
result carries **one** note per drain quoting the build string the reply's envelope carries
(`bridge < m2.20 (build X): no packed params, positions unavailable (N via-11 event(s) …)`; the
build is `resp.build()`, `"unknown"` if the envelope lacked it). The present-but-`null` case —
the m2.20 unreadable pointer — is instead marked **per event** as `params:{"unreadable":true}`
with no bridge-age note: the params path existed, the buffer did not. Slots 12/13/81 never carry
the key and never trigger either marker. Abuse note: an m2.20-style reply with an unexplained
*absent* key on a via-11 event is still read as "old bridge" — absence is the only signal the
reply shape gives.

**The local filter** is applied after the drain: `namePrefix`/`names` on the event name, and
`within` on the decoded `position` first, else on a non-null non-zero `origin`. An event with
neither can never match a `within` filter; it is still consumed and counted in `unmatched`, and
the result carries a note saying how many were like that (the worldent origin `[0,0,0]` and the
absent/null origin cases).

**[V live]** (2026-10-16, the acceptance flow — see Live acceptance): `run_map` loaded the
addon's `soundevents_addon.vsndevts_c` automatically and the drain returned the custom names with
exact packed-params positions; the soundscape showed only its PARENT event; the timer's first
fire matched InitialDelay + RefireTime. Still mock-responder-only: the truncated/`dropped`
reply-budget shapes, the old-bridge and unreadable-params markers, and `installOnly` against a
live bridge (the acceptance armed the wrap and never hit those shapes).

Verified by `tests/sound_runtime.rs` (23: registration/spec/closed schema, every bridge field
through with `matched`/`unmatched` arithmetic, `max` always sent, `resolve:false`, the live
captured buffer decoded to `position [944,160,64]`, `truncated`/`dropped` notes, `within` on
position-then-origin with the unmatched note, `namePrefix`+`names`, `installOnly` sending only
`sound.status` with just a deadline, an unarmed wrap, and the old-bridge markers:
`an_m2_19_bridge_reply_notes_the_missing_params_for_via_11_only` (the note once, with the build,
and never for a via-12-only reply),
`an_m2_20_reply_with_a_null_params_is_marked_unreadable_but_not_old_bridge`,
`the_bridge_age_note_quotes_the_envelope_build_the_reply_carried`) and the module's own 11 unit
tests (the decoder: a multi-entry walk, a truncated copy, a walk overrun, a `len` mismatch, bad
hex, `null` params, a non-finite float, the sphere's position-first rule, the arg bounds).
NEGATIVE: `a_bridge_without_sound_drain_is_not_implemented_and_sends_nothing`,
`max_out_of_bounds_is_refused_with_nothing_sent` (0/257/-1/1.5/"8"),
`a_non_positive_within_radius_is_refused` (0/-5/0.0 and a 2-number center),
`names_above_the_cap_and_ill_typed_entries_are_refused` (33/empty/non-string),
`unknown_arguments_are_refused_at_the_top_level_and_nested` (top level, `filter`, `within`),
`filter_with_install_only_is_refused_with_nothing_sent`,
`a_prerequisite_failed_passes_through_unchanged` (-32017 with the bridge's own `data.reason`),
`a_reply_without_events_is_an_internal_error_never_an_empty_success` (missing **and** ill-typed
`events`), `truncated_overrunning_and_mismatched_params_stay_undecoded_with_a_null_position` (all
of them keep `position:null` — the unreadable one as `params:{"unreadable":true}`, the keyless
via-12 event as plain `null` — and a `within` filter over them matches nothing while noting why).

### Tests

Hermetic against the fixtures (no game tree needed; `#[ignore]` runs need the real installs and
FAIL loudly without them): `tests/common/mod.rs`'s `SoundTree` (a synthetic csgo+core VPK pair
with three `soundevents/*.vsndevts_c` blocks plus the addon's `.vsndevts` and `.vsndevts_c`, and
`write_vsndevts_c`), `fixtures/soundevents/soundevents_addon.vsndevts` (the shipped template,
also the source of `edit_soundevents`' header), and `fixtures/fgd/core/base_mini.fgd` (carries
the real-FGD sound classes: `EnableDisable`, `ambient_generic`, `point_soundevent`, the
`snd_event_*` shapes, `env_soundscape`/`snd_soundscape`, `trigger_soundscape` (Solid, base
`Trigger`, **no** tag), `logic_auto`, `logic_timer` — with the deliberate exception that the
fixture's `snd_soundscape` carries no own `tags()` so the base-chain resolution is positively
exercised by a non-ignored test). Per binary:

- `tests/soundevents_index.rs` — the fixture's precedence/copy merge, the string-or-array
  normalisation, `resolve_chain` incl. the cycle bound, the suggestion ranking incl. a padded
  request, `fresh()` over the files/paks/directories, the rescan that survives a deleted pak,
  the `stale_source` rule, the `vpk_unchanged` gate, the rescan-after-invalidate against a
  garbage-pak-with-restored-mtime and the full rebuild on a moved pak, the corrupt-block /
  no-DATA / bad-DATA / absent-directory / unparseable-source notes, the missing-VPK errors,
  `index_for`'s cache + invalidate hook, plus the two `#[ignore]`d corpus tests (the real-tree
  numbers above; run with `cargo test -p hammer-mcp --test soundevents_index -- --ignored`).
- `tests/soundevents_query.rs` — the list filters, paging, the summary, the detail route incl.
  the merge override order and the cycle, `rebuilt` across the cache, the `Stale` and padded
  verdicts, and the argument / not-found / note negatives (`name`+`query`, `limit`/`offset`
  bounds, `source`/empty filters, unknown arguments, `VpkMissing` on both routes,
  `SoundEventNotFound` with suggestions).
- `tests/soundevents_edit.rs` — the op round trip (add/merge/replace/rename-with-references/
  remove), **comment preservation** on the real addon template (every comment line
  byte-for-byte, header unchanged, CRLF intact), `createFile`'s exact header, `dryRun` writes
  nothing, backup path, the index invalidation on write only, the real-index checks
  (`base_and_children_checks_run_through_the_real_index`: a VPK-only base resolves, an unknown
  child warns, an unknown base is refused with the index's suggestions;
  `unknown_property_keys_warn_against_the_real_corpus`; `an_empty_corpus_skips_the_key_check`),
  the unbuildable-index policy (`an_unbuildable_index_refuses_the_edit`: `VpkMissing` + nothing
  written at the default `validate:true`, a proceed-with-warning at `validate:false`), a rename
  leaving a same-text property value alone on both sides
  (`a_rename_leaves_a_property_value_holding_the_old_name_untouched`), a rename to `1up`
  quoted and re-parsed (`a_rename_to_a_digit_leading_name_quotes_the_key_and_reparses`), argument
  errors before the index (`a_valid_set_then_an_invalid_op_refuses_arguments_and_not_the_index`),
  the two concurrent edits landing (`two_concurrent_edits_of_one_file_both_land`, the
  soundevents gate), and the negatives: a
  comment inside the edited event (`commentsDropped` names it), unknown `base` (nothing
  written), no `type`/`base`, exists-without-overwrite/merge, bad names (space / `/` / 128
  chars), non-object and non-convertible `properties` (naming the key), referenced `remove`
  refusal + `force` warning, outside-addon/`..`/wrong-extension paths, missing header (parse
  error line 1), a parse error at line N ({line, col}, nothing written), the injected broken
  splice (`InternalError`, nothing written), argument negatives (`ops` empty/missing/unknown
  keys, self-rename, `dryRun` type).
- `tests/soundevents_compile.rs` — ok (+ real event count from a synthetic `_c` + index
  invalidation), the failure shape as `ok:false` result with `errors`, an unreadable output
  (`events: null` + note), `the_event_count_counts_only_object_root_members`, missing file, no
  container, outside-addon/timeout argument refusals.
- `tests/sound_entities.rs` (33) — the mock inventory in `create_sound_entities` above; the
  sound verdicts are pinned per test server through the shared `SoundCheckSource` state seam.
- `tests/sound_runtime.rs` (23 + 11 unit) — the drain inventory in `drain_sounds` above.
- `tests/fgd.rs` — `sound_classes_are_recognized_by_the_tags_helper_through_the_chain`,
  `sound_keys_lists_only_the_sound_typed_keyvalues`, the negatives
  `is_sound_class_is_false_without_the_tag` (a class with another tag, a class that is not in
  the database, and the sharp `trigger_soundscape`: a `soundscape` key must not make a sound
  class) and the `#[ignore]`d `real_game_sound_classes` (the real tree: 15 tagged classes
  named, the tag-filter population — 29 point/solid classes measured 2026-10-16 — and
  `soundName`/`soundevent`/`message`).
- `tests/entity_classes.rs` — `list_tag_filter_selects_the_tagged_classes` (the exact `Sound`
  list, case-insensitivity, the `kind` composition, a second tag, `tag:"Nope"` → `total: 0`,
  non-string/blank → `InvalidArguments`).
- `tests/entities_live.rs` — the sound-key hook cases: the create refusal (nothing sent,
  suggestions/sources on the error), `allowUnknownSoundEvent` writing with a warning, a `Known`
  name writing silently, a `SourceOnly` warning, the set refusal, `force` writing with a
  warning, a revert skipping the lookup (against an `Unbuildable` source, so any lookup would
  have refused), the `FilePath` and `Empty` warnings, the `VpkMissing` refusal + `force` pass,
  the FGD-less "did not run" warning (also asserting that a `targetname`-only edit is not
  warned about, that the warning names the key it could not type, and — against a `func_door`
  edit of `noise1`/`noise2` — that the FGD-free key list is the real FGD's sound-typed key names
  and not just the three event keys), non-sound keys being
  ignored, a `Stale` name warning and writing, a whitespace-padded name refused with the
  trimmed suggestion and the "leading/trailing whitespace" sentence, and the multi-thread seam
  test. `src/tools/sound_check.rs` also has unit tests for the refusal payloads, the whitespace
  sentence and the key-name predicate (the sound-typed names incl. `noise1`/`noise2`,
  `startsound`, `use_sound`; `targetname`, `soundscape`, `model` are not on it).

Fixture class counts: `fixtures/fgd/core/base_mini.fgd`'s additions moved the class counts in
`tests/entity_classes.rs` to 27 (13 point, 6 solid, 8 base) / 19 point+solid; the tagged
populations are 7 `Sound` and 2 `Logic`.

### Live acceptance (2026-10-16)

The orchestrator's acceptance flow ran the block end to end against the real addon tree
(bridge m2.19 + m2.20) and recorded these verified facts:

- **29 classes** carry `tags( Sound )` in `game/core/base.fgd`
  (`list_entity_classes {tag:"Sound"}`).
- **`run_map` loads the addon's `soundevents_addon.vsndevts_c` automatically** — no extra mount
  or compile step — and `drain_sounds` returns the custom names.
- **Positions decoded from the packed params (bridge m2.20) were exact**: a `startOnSpawn`
  `snd_event_point` fired at load at its Hammer origin `[944,32,80]`; a `logic_auto` delay 3 →
  `StartSound` at t = 3.08 s at `[1136,32,80]`; a `logic_timer` `RefireTime 5` fired at 5.88 s
  and 10.88 s at `[944,288,80]`; the soundscape parent event fired at load. Control: a
  `snd_event_point` **without** `startOnSpawn` fired only on its timer.
- **A soundscape (`enablesoundevent 1`) shows only its PARENT event in `drain_sounds`**; its
  children (`soundevent_01`) start inside the operator stack and are never visible to the wrap.
- **`logic_timer` first fire = InitialDelay + RefireTime.**
- `resourcecompiler` checks **syntax only**: unknown base/children/keys/vsnds compile OK, so
  `edit_soundevents`' checks are the only ones (research/sounds-re.md Q1).
- The live bridge check is `python3 -I bridge/test_sound_params.py` (bridge m2.20, tools session
  up): PASS (exit 0) needs at least one via-11 event whose `params` decoded; zero decoded params,
  or an unreachable bridge, is `RESULT: NOT RUN` (exit 2), never a pass; a failed check is exit 1.
  `--static` checks the bridge source against the pins without a game.

## Selection (bridge m3.12, `src/tools/selection_live.rs`)

`get_selection {}` (ReadOnly, `selection.get`) and `set_selection {op, nodeIDs? | faces?}` (Mutating,
`selection.set`) read and change what is highlighted in the LIVE Hammer document. Capability-gated:
a bridge without `selection.*` answers `NotImplemented{data.missing}` and nothing is sent.

- **Wire shapes.** `get` -> `{mode: groups|objects|meshes|vertices|edges|faces|mixed|tile|null, count,
  nodeIDs, faces:[{nodeID, faces:[i..]}], source:"live"}`; `faces` is filled only in faces mode, and in the
  vertices/edges/mixed/tile modes only `count` is reported. `set` takes `op` replace|add|remove|clear and
  exactly one of `nodeIDs` / `faces` (clear takes neither) and answers `{requested, applied, modeChanged,
  undoGroupOpened:false, notApplied?, note?, saved:false, source:"live"}`; `applied` is the `get` shape
  re-read after the call.
- **Robustness members.** `unmapped` (selected items the bridge could not map to a node id or face list;
  0 normally), `truncated:true` (lists cut at 16384 entries, `count` stays true). In a selection mode Hammer
  cannot name, `get` reads mode null / count 0 and `set` refuses every op including `clear` with
  `-32603 {reason:"selection mode unknown"}` before any Hammer call. If the selection changed but the
  re-read failed, `set` still succeeds with `applied:null` and `appliedError`.
- **Face indices are `get_mesh`'s**: the bridge numbers a mesh's faces in Hammer's own face order
  (`CollectHandles`), which is also the order the saved `.vmap` stores; a model can pair a selected face
  with `get_mesh` output and send them to `set_mesh_material {faces}`.
- **Not an edit.** Selection opens no undo group and does not dirty the document, so `set_selection` does
  not touch the unsaved-edit counter and `undo_last` never sees it. It still holds the server-wide write gate
  (`HammerState::write_gate`) for its read-document / send sequence and sends `expectDoc` from the shared
  `require_document` read; `get_selection` takes no gate.
- **Mode switches.** `set` with `nodeIDs` switches Hammer to objects mode unless it is already in
  groups/objects/meshes mode; with `faces` it switches to faces mode. Hammer converts nothing on a switch,
  so after one (`modeChanged: true`) `add`/`remove` are relative to an empty selection.
- **Filtering is not an error.** Hammer may skip locked/hidden nodes and meshes that ignore face selection;
  `applied` is then smaller than `requested` and `notApplied` lists what was left out (replace/add only).
- **Refusals** (all `-32602`, nothing changed, retry-safe): unknown nodeIDs (`data.unknown`), a face target
  that is not a mesh (`data.reason`), a face index >= the mesh's face count (`data.badIndices`,
  `data.faceCount`), more than 4096 nodeIDs or 65536 face indices, negative or non-integer ids/indices. The
  tool refuses the shape problems locally, before anything is sent.
- Tests: `tests/selection_live.rs` over the mock's selection model (`MockBuilder::hammer_selection`,
  `hammer_ui_select_*`, `hammer_selection_filters`).

## Play-test tools (`src/tools/game_live.rs`, `src/tools/play_test.rs`)

For testing a map in the tools session's engine without a second bridge client (the bridge takes
one, so client-mcp cannot attach while hammer-mcp runs). Capability-gated like the rest. The same
names client-mcp has, over the same code (`crates/cs2-mcp-common/src/play/`: `state`, `sequence`,
`input`, `wait`, `predicate`): an agent never writes a python driver (`drive.py`, `endtest.py`).
Refusals say `NoMapLoaded` with `useInstead run_map` when the engine has no map.

- `game_state {samples?=1 (<=2000), intervalMs?=16 (<=1000)}` (ReadOnly, `game.local`): the local
  player as the client sees it, once or sampled. With samples > 1 it adds `originStepStats` per
  axis (mean/stddev/min/max step, `reversals` = sign flips of the step, `zeroSteps`): how a view
  jitters shows up as reversals while riding something scripted. `game_local` is its deprecated
  alias (`Registry::alias`; the old name answers with `deprecated:{alias,use}`).
- Round-start timing (live evidence, acceptance run 3, 2026-10-26; it cost two compile cycles):
  `logic_auto OnMapSpawn` fires at map load (inside the freeze period), `logic_auto OnMultiNewRound
  ... Open(delay)` did not move a `func_movelinear`, and a `logic_eventlistener` on
  `round_freeze_end` did. `get_entity_class` / `list_entity_classes` carry this as `playNote` for
  those three classes (`entity_classes::play_note`).
- `entity_find {class?, name?, max?}` (ReadOnly, `game.entities`; live-fix D): entities of the RUNNING
  game (entindex, class, name, origin, angles, health, toggleState), shared with client-mcp's tool
  of the same name (`play::entities`). It is the game, `find_entities` is the Hammer
  DOCUMENT: both descriptions say so. A play tool: `ADDON_EXEMPT_TOOLS`, `NoMapLoaded` with
  `useInstead run_map` when no map runs. Registered `ReadOnly`, so it is in neither `LIVE_MUTATORS`
  nor `NOT_DOCUMENT_EDITS` (those two partition the `Mutating` tools only; it edits no document).
- `game_rules {}` (ReadOnly, `game.rules`; play follow-ups): the round and warmup state of the
  running game (warmup, freeze, teamIntro, gameRestart, roundsPlayed, roundStartTime, ...), shared
  with client-mcp's tool of the same name (`play::state::rules`; client-mcp's output is unchanged).
  Addon-exempt, `NoMapLoaded` with `useInstead run_map` when no map runs.
- `game_state` adds a `hint` (and a `next` for it) when the player is on team 0 or dead:
  `{problem, fix {tool:"console_exec", args}, why}` (`team.rs`). `run_map {team:"ct"|"t"}` does the
  fix after the load: `jointeam 3 1` / `jointeam 2 1` through `console_exec`'s own code (so its
  guard, `vtm_gate` and bridge-or-vtm route), then polls `game.local` (150 ms, <= 2.5 s) until the
  player is alive on the team; a player left dead on the team gets one `mp_restartgame 1`
  (`restartedGame`; `restartIfDead:false` never restarts, `team.note` says to do it by hand) and a wait of up to 15 s / the budget. A load that was not confirmed reports `team {requested, skipped:"load not confirmed"}`. `game_state`'s hint treats team 1 (spectator) like team 0 (`jointeam`). The result's `team {requested, joined,
  alive, restartedGame, commands, waitedMs, note}` never fails the load. Without `team` a note on
  the result says where the player stands.
- `input_set` / `input_clear` (Mutating, `input.set` / `input.clear`): held virtual buttons and a
  view-angle override, the same bridge methods client-mcp drives. Traps (FIXME 26): while an
  `angles` override is set, console `setang` is ignored (`angles: null` or `input_clear` gives the
  view back); `release` + `tap` of the same held button in one call re-presses it only on bridge
  m3.15+ (older bridges: two calls).
  The `input_clear` reply's `held` is always `[]` (the bridge's reply is the status read before the
  next frame applies the release and could still list the buttons; the tool moves them to
  `releasedHeld`).
- `input_sequence {steps}`: up to 32 input.set steps, each held `holdTicks` game ticks; clears at
  the end. Identical to client-mcp's (shared `play::sequence`).
- `play_test {steps, keepInput?}` (Mutating): a scripted run in one call. Steps are tagged by `kind`:
  `input {steps}` (as input_sequence, but the keys STAY held for the next step), `wait_ms {ms}`,
  `wait_state {path, <op>, timeoutMs?, pollMs?}` (polls game.local until it holds), `wait_log
  {pattern, since?: step|start, timeoutMs?}` (regex over console.log lines; `run_map`'s
  `ConsoleWatch`), `expect_state {path, <op>}`. `<op>` is exactly one of `eq ne gt ge lt le near
  (+tolerance) contains exists`; `path` is dot/index (`player.origin[0]`, `player.alive`). Every step
  is validated before anything is sent; the run stops at the first failed step
  (`endedBy: failed`) or when the budget cannot cover the next one (`endedBy: budget`); input is
  cleared at the end however it ended (not after a passing `keepInput` run). Returns `{passed,
  endedBy, steps:[{step, kind, label, ok, ms, result|error}], failedStep, inputCleared}`. A
  `wait_log` with the default `since: step` sees only lines printed after that step began: when the
  line can appear during an earlier step (a long `wait_state`), use `since: start`.

  Worked example (ti_convoy: drive, wait for the plant, check the round):

  ```json
  {"steps": [
    {"kind": "input", "steps": [{"press": ["forward"], "angles": [0, 90, 0], "holdTicks": 1}]},
    {"kind": "wait_state", "path": "player.origin[1]", "gt": 1200, "timeoutMs": 20000},
    {"kind": "input", "steps": [{"angles": [0, 180, 0], "holdTicks": 1}]},
    {"kind": "wait_log", "pattern": "package planted", "since": "start", "timeoutMs": 15000},
    {"kind": "expect_state", "path": "player.alive", "eq": true}
  ]}
  ```

  **Entity steps (live-fix D).** `wait_entity {class?, name?, index?, path, <op>, timeoutMs?,
  pollMs?}` and `expect_entity {class?, name?, index?, path, <op>}` run the same predicate engine
  over ONE entity of the running game, found with `game.entities` (the `entity_find` bridge call:
  class and/or name, at least one; `index` picks the n-th match, default 0 = the first, max 255).
  `path` is a field of the entity (`origin[0]`, `origin[2]`, `health`, `angles[1]`, `toggleState`,
  `name`, `entindex`). No match yet keeps `wait_entity` polling (the entity may spawn) and fails
  `expect_entity`; the result carries `entity {entindex, class, name}`. "The mover's origin x passes
  1500" is the one step `{kind:"wait_entity", class:"func_movelinear", path:"origin[0]", gt:1500}`.
  A `play_test` with an entity step on a bridge that does not advertise `game.entities` is refused
  `NotImplemented{missing}` before any step runs (nothing is sent).

  **Freeze period (live-fix D, lost input).** An `input` step that PRESSES or TAPS a key first
  reads `game.rules` (when the bridge advertises it; release-only and angles-only steps do not),
  at most once per run: while `freeze`, `teamIntro` or `gameRestart` is true it waits (the step's
  `lockWaitMs`, 0..=60000, 0 = no wait; 100 ms polls, within the budget) before
  sending, and reports `movementLock {phase, waitedMs, stillLocked, lockWaitMs, lockWaitSource}`
  plus a `note`. Without an explicit `lockWaitMs` the limit is 20000, raised to `mp_freezetime`
  (read once with `cvar.get` when a lock is first seen; `game.rules` has no remaining time) + 5 s
  when that is longer, capped at 60000; `lockWaitSource` says `lockWaitMs`, `default` or
  `mp_freezetime`. An explicit `lockWaitMs` always wins. Keys pressed
  DURING a lock stay dead after it ends (call 847), so a step still locked at the limit FAILS
  (`movementLocked:true`, `inputSent:false`) and sends nothing; raise `lockWaitMs` or shorten the
  lock. A step that held a move key for >= 8 ticks and moved < 1 unit gets a
  `note` naming the reasons it can read (rules lock, dead, team 0, else geometry). Cause analysis in
  `workflows/streamline/FIXME.md` "Live-fix D": the transcript points at the freeze period after
  `mp_restartgame`, not at the bridge's input hook.

  Not in `play_test` yet: a `console` step (WP1.3 adds `console_exec` and its refusal guard in
  `cs2-mcp-common`; a step can call that function when both have merged) and teleports of bots.

`compile_map` takes `mode` (presets `full` | `nav` | `novis`) or `stages: [..]` (exclusive): any subset
of `world phys vis bakelighting nav` in any order, normalised to the compiler's order and sent as the
matching `-flag`s. These five are the only flags with a spelling shown in the repo (the recipe,
`source2-mcp-research.md`); the step table also has `lod gridnav sareverb sapaths sacustomdata`, but
no flag spelling is known, so they are refused (`InvalidParams`, `data.didYouMean` for a typo such as
`vsi`). Presets: `full` = no stage flags (reported as all five), `nav` = `[nav]`, `novis` =
`[world phys bakelighting nav]` (about 2.5 min instead of ~10 for ti_convoy). Any list without `world`
needs an existing vpk (generalised `nav` gate). The result and every poll carry the resolved `stages`.
Release builds use `full`. bridge compile whose state is `ok` but whose `logTail` has no parsable compiled count re-reads
`/log/host` before reporting (the `9m:35s` OK line used to come back `failed`).

## Addon selection (WP1.1, `src/tools/addon.rs`, `HammerState::addon_sel`)

One server serves any addon under `content/csgo_addons`. The effective addon is resolved at every
use (`state.addon()`, `state.content()`; never cache either): an explicit `set_addon {addon}` wins
until `set_addon {addon:null}`; else the addon of the active Hammer document (derived from its host
path under `content|game/csgo_addons/<addon>/`); else `--addon`. The document-derived value is
refreshed wherever a tool reads `doc.active` (`live_common::read_active_document`) and by
`ToolState::before_call` before EVERY tool except `state::ADDON_EXEMPT_TOOLS` (the selection/job
tools, bridge-only edits that never touch an addon path, and the handlers that read `doc.active`
themselves before their first addon use: `create_entities`, `set_entity_keyvalues`, `create_props`,
the path forms of the file-route reads via `entities_file::resolve_document`), so a new tool is
covered by default (a missing exempt entry costs one extra `doc.active`, never a stale addon). The re-check is a
`doc.active` read through `live_common::read_active_document_locked` that waits for the bridge lock
(up to 10 s, clamped to what remains of the tool budget; never skipped because another call is busy).
For a tool with bridge methods, `Registry::call` first runs the capability gate (`require_caps_within`),
which waits for the same bridge lock for at most `CAPS_LOCK_WAIT` (10 s, clamped to the budget) and
otherwise answers the retry-safe `BridgeBusy` (nothing sent). Only tools with bridge methods pass
the gate: `compile_map`, `run_map`, `compile_asset`, `job_status` and `job_cancel` register
`bridge_methods: &[]` and never do. The "within 10 s" is not a hard bound: after the gate drops the
lock, `before_call` may wait up to another `ADDON_LOCK_WAIT` (10 s) for it, so the worst case before an
`AddonCheckFailed`/`BridgeBusy` refusal is about 20 s. And the handler's own `state.bridge.lock().await`
is still unbounded: a holder that takes the lock between the gate and the handler (TOCTOU) makes the
call queue behind it again (FIXME "Live-fix B": a bounded `lock_bridge(ctx)` helper for handlers).
Only bridge-backed read-only tools are throttled (1.5 s: they read `doc.active` themselves); file
tools and every mutator re-check on each call, so the call right after a document switch sees the
new addon. A pinned addon skips the check. When the check cannot tell (lock wait timed out, bridge
error other than "Hammer is not up"), `before_call` returns `Result<Option<note>>`: a Mutating/Writer
tool is refused with the retry-safe `AddonCheckFailed` (nothing ran); a read-only tool runs on the last
known addon and its result carries `addonNote`. "Hammer is not up" is a definite answer (no document:
pin or `--addon`). File writers report `addon` + `addonSource` in their result. A finished compile
job poll whose job addon differs from the current one suggests `set_addon {addon: job.addon}` instead
of the addon-relative `run_map`/`compile_map {path}` steps.
Multi-step handlers snapshot `state.content()` once per call (compile_map keeps the addon in its
`MapTarget` and `CompileJob`, so a later poll/`running_for_path` never re-points a job).
`session_status` reports `addon:{name, source, flag, pinned,
documentAddon}`; `set_addon` with no argument lists the addons available; an unknown name is
`AddonNotFound` with `data.didYouMean` and changes nothing.
Caches are keyed by addon: the model catalog and the soundevent index carry the addon they were
built for and are rebuilt (never rescanned from) when it differs.
ModelDoc is the one thing the server cannot switch: the container is started with `-addon <a>`
(`bridge/runtime.sh` also labels it `cs2mcp.addon=<a>`). `model_open` reads that
(`DockerRuntime::container_addon`: label, else the `-addon` word of the launch command; unreadable =
passes) and refuses with `AddonMismatch` (`selectedAddon`, `modeldocAddon`, `hint` = restart ModelDoc
on the selected addon, `useInstead` = the optional `set_addon` to ModelDoc's addon) when it differs from the selection; it also checks that the
document the editor reports is the file asked for (`WrongDocumentOpened`, compared with canonical
paths; not retry-safe: the editor already switched). `model_edit`/`model_create`
are correct file writes whatever ModelDoc runs, so they only add a result `warnings` entry.
`other_addon_hint` (run_map/compile_map refusals) names how the selection was made (pinned by
`set_addon`, the active document, or `--addon`).

## apply_layout (`src/tools/layout.rs`, WP1.2)

Many meshes and entities from ONE JSON layout (ti_convoy FIXME 8, 13: the agent used to paste a
generator script's output into `create_block` one call per mesh). Mutating; needs `mesh.create`,
`edit.batch`, `entity.create`. Registered last (`LAYOUT_TOOL_ORDER`).

Arguments: `path` (a `.json` file) XOR `layout` (the same JSON inline); `dryRun?=false`;
`name?="MCP: apply layout"`; `strict?` and `allowUnknownSoundEvent?` (as `create_entities`).

```json
{"meshes":   [{"kind":"block", "id":"road",  "min":[0,0,0], "max":[512,64,8], "material":"..."},
              {"kind":"mesh",  "id":"ramp",  "vertices":[...], "faces":[...], "orientOutward":true}],
 "entities": [{"id":"spawn", "class":"info_target", "origin":[0,0,32], "keyValues":{"targetname":"s1"}}]}
```

- A `meshes` item is `kind:"block"` (exactly `create_block`'s arguments: `shape`, `min`/`max`,
  `riseToward`, `center`/`radius`/`height`/`sides`, `material`, `topMaterial`...) or `kind:"mesh"`
  (exactly `create_mesh`'s). An `entities` item is a `create_entities` item
  (`class`, `origin`, `angles?`, `keyValues?`). Every item may carry an `id` (a unique string of
  at most 64 bytes) that the result maps to the `nodeID`. Any other key is refused: the
  validators are the single tools' own (`mesh_live::prepare_create_block`, `prepare_create_mesh`,
  `prepare_wire`; `entities_live::create_op`), there is no second schema.
- `path`: host absolute, Windows (through `--path-map`), or relative (tried under the active
  addon's content dir, then the project root). It must resolve (symlinks followed) inside the
  project root and end in `.json`; `..` segments are refused. Both or neither of `path`/`layout`
  is refused.
- Validation is whole-layout: every bad item is collected (`data.errors[]` = `{section, index, id,
  message}`, `errorCount`), nothing is sent. Limits, refused up front with the number:
  `MAX_LAYOUT_MESHES` = 32 meshes (one `mesh.create` request each inside one call's budget), 64
  entities (one `edit.batch`), 4 MiB of mesh parameters in total. Split bigger layouts.
- `dryRun:true` returns `{valid, wouldCreate{meshes,entities,total}, bridgeRequests, undoGroups,
  bounds, items[]}` and sends NOTHING to the bridge from this tool (the server-wide addon pre-check may still read `doc.active`).
- Apply: one `mesh.create` per mesh, then ONE `edit.batch` with all the entities. **Undo groups**:
  the bridge's `edit.batch` runs only `entity.create` / `entity.setKeyValues` ops
  (`crates/cs2-bridge/src/ops/edit.rs`) and `mesh.create` opens its own group
  (`ops/mesh.rs op_mesh_create`), so the meshes cannot share a group. `undoGroups` = meshes (+1 for
  the entity batch); `undo_last` pops one per call. `name` is echoed as `requestedUndoLabel` only;
  the bridge labels the groups itself. Every request carries the `expectDoc` of the layout's one initial `doc.active` read (the bridge's
  fingerprint is structural, so edits do not move it); a document switch mid-way is refused by the
  bridge as `DocumentChanged`, nothing written, with the progress report below.
- Result: `{created:[{id, kind: block|mesh|entity, section, index, nodeID, ...}], count, meshes,
  entities, bridgeRequests, undoGroups, undoNote, requestedUndoLabel, unsavedBridgeEdits, saved:false,
  missingNodeIDs, warnings}`; `next` is `save_map` from the registry's mutator hook (or
  `find_entities` then `save_map` when `missingNodeIDs` is not empty). Warnings: Hammer created fewer faces than were sent, a reply without
  a usable nodeID, keyvalues not applied.
- A failure mid-way: the error carries `created` (what exists, with nodeIDs; entities a partial
  `edit.batch` made before its `failedIndex` are included), `failedItem`, `failedItemMayExist`,
  `notSent`, `undoGroups`, `recovery` and `unsavedBridgeEdits`. `undoGroups` counts only groups that
  certainly exist. A failed request that may have landed (`outcomeUnknown`: a timeout, `-32603`...)
  is NOT counted and NOT in `notSent`: `failedItemMayExist:true` and `recovery` say to call
  `undo_last {undoGroups}` times, check whether `failedItem` exists, and if so once more. A `-32602`
  pre-write refusal or a `DocumentChanged` is not maybe-executed.
- Tests: `tests/layout.rs` (mock bridge): the 3 blocks + 1 mesh + 1 entity layout, file route,
  every-bad-item, dry run, path outside the root, path/layout exclusivity, limits, and the mid-way
  failures (-32603, -32602, timeout, DocumentChanged, budget, edit.batch -32603 and -32012 with
  rows, a reply without nodeID).

## Console (`src/tools/console_live.rs`)

- `console_exec {command, waitMs?=1500 (<=10000), waitFor?: regex, maxLines?=200}` (Mutating, in
  `NOT_DOCUMENT_EDITS`; in `ADDON_EXEMPT_TOOLS`: it edits no document and its paths hang off the
  app root, not the addon): one console line in the engine, with its output read back. Order:
  argument check; the shared refusal guard (`cs2_mcp_common::console::refusal`, also client-mcp's:
  `quit`, `exit`, `disconnect`, `maps`, ..., the first token of every `;` segment cut at the
  engine's break characters `{}()':"` so `quit()` is `quit`, alias and bind bodies; an `exec <cfg>`
  file is not read) -> `ConsoleCommandRefused`; then the hammer-only map-changing list
  (`MAP_CHANGING_COMMANDS`: `map`, `changelevel`, `map_workshop`, `host_workshop_map`,
  `ds_workshop_changelevel`, `connect`, `retry`, `restart`, `playdemo`, `startupmenu`) ->
  `ConsoleCommandRefused` with `data.useInstead` = `run_map`; nothing is sent in either case.
  Everything after is one critical section under `vtm_gate` (the gate `run_map` posts under): a
  fresh `game.local` read, then `settle_unresolved_previous` (the load-in-flight decision `run_map`
  uses: a finished load settles, an unfinished one is `EngineBusy`), then the level state from the
  last level line in the console.log tail (`Loading map "` = loading -> `EngineBusy`, unless
  console.log has been silent for `SETTLE_STALE_MS` (60 s): then the load is taken as stale, the line
  runs with a note, and the busy message names `run_map {unload:true}` as the reset; `Spawn Server:
  <empty>` or `game.local` inGame false = idle level -> the line runs with a note: game_type,
  game_mode, sv_cheats and mp_* are set there before `run_map`).
  Two length limits, both refused up front as `InvalidArguments` with nothing sent. **The engine
  drops any single command over 510 bytes** (`WARNING: Command too long... ignoring!`) and any
  command whose name (first token) is over 128 bytes (`Console command too long.`); the limit is
  per `;`-separated command, not per line (live 2026-10-26: a 511-byte `hostname "<500 z>"` was
  dropped, a 702-byte chain of short commands ran in full). `console_exec` measures each segment
  (trimmed) with `cs2_mcp_common::console::{command_segments, engine_limit_violation}`, the very
  segmentation the refusal guard uses: it cuts on every `;`, `\n` and `\r`, quoted or not, so a
  `;` inside quotes also cuts and can under-count one engine command (the readback still catches
  it). The bridge's whole-line buffer is a separate cap of 1023 bytes (`BRIDGE_COMMAND_MAX`); the
  message says which limit was hit. There is no long-line vtm fallback. Either engine line in the
  readback (`Command too long` or `Console command too long`) is a `deliveryWarning`.
  Route: the bridge `console.exec` when advertised, else
  (also when the bridge answers `-32011`, the session gate: `-tools` or `-testscript` since m3.13,
  or cannot be reached: a `notes` entry) the VToolMessage command channel (`run_map`'s
  `post_command`); `route` is `bridge` or `vtm`. A marker command `cs2mcp_mark_<unique>` is sent
  right after the line on the same route (client-mcp's `Unknown command: <marker>` confirm
  technique): the engine runs its command buffer in order, so the marker line ends the output.
  Readback: whole lines console.log gained after the call started (an unterminated last line is
  never matched or returned, `console.partialTail`; UTF-8 is decoded across polls; a shrunk or
  replaced file restarts at 0 and sets `console.rotated`), polled every 50 ms. `waitFor` never
  matches the engine's own echo of a posted command (the block `[ToolFramework2] Handling
  VToolMessage:` / `{` / `_message` / `_data = "<command>"` / `}`, which repeats the command text;
  `echo_block_mask`) nor the marker's line; those lines stay in `lines`. **In a `-tools`
  session, on the bridge route, `echo` and cvar prints never reach console.log** (only
  `status`-class output and engine errors do), so `waitFor` on `echo` output times out: read cvars
  with `cvar_get`, and match output a command really logs (a `status` line, an `Unknown command:`
  error). `echo ...` or a bare cvar name on the bridge route gets a note saying so. Without `waitFor`
  the output is cut at the marker. With `waitFor`, the marker does not end the wait: the regex is
  matched over every complete line, also those printed after the command returned (a timer, a
  round start), until `waitMs`; then `endedBy: timeout` and `markerSeen` says whether the marker
  arrived. `endedBy`: `match` (waitFor matched), `marker`, `quiet` (no marker could be sent, or
  on the vtm route, which may not echo it: output arrived and was silent for 500 ms), `timeout` (waitMs or the tool budget), `noFile` (no
  console.log: `consoleNote`). `lines` are the LAST `maxLines` lines (`truncated`, `lineCount`).
  Result `{route, command, lines, lineCount, truncated, endedBy, matched?/matchedLine?, waitedMs,
  console, markerSent, notes?, deliveryWarning?, consoleNote?, bridgeRefused?, settledPreviousLoad?,
  vtmsend?}`; there is no `ok` field (a command's success is not known; read its output). A
  `delivered:false` exec is `deliveryWarning` (not repeated; no marker is posted after it). A vtm
  route that returns zero lines says so in `notes` (cvar queries print nothing there: use
  `cvar_get`). Not for load/unload (`run_map`).
- `cvar_get {names (1..64)}` (ReadOnly, bridge `cvar.get` since m3.13, capability-gated): per name
  `{name, found, type, value, default, flags, flagsRaw}`; devonly cvars read as `found:false`, a
  `protected` cvar is `redacted:true` with no value, and so is any cvar by name (the tool layer,
  `cs2_mcp_common::console::redact_cvar_reply`: `rcon_password`, `sv_password`,
  `sv_setsteamaccount` and any name containing `password` or `token`, case-insensitive;
  `rcon_password` has no protected flag in this build; a name-redacted entry also carries
  `redactedBy:"name"` and a `redactedNote`, and a `found:false` entry is never marked redacted). Still best-effort hygiene, not a secret
  boundary: `console_exec <name>` can print a value (only setting `rcon_password` is refused by the guard). This is the way to read a cvar:
  `console_exec` of a bare cvar name prints nothing on the vtm route.
- Live-verified (LIVE-P1 section 7, build 2000933): in a `-tools` session the bridge route's
  `status`-class output and engine errors reach console.log, `echo`, bare cvar names, `cvarlist`,
  `find` and `help` do not (so there is no way to print a cvar there: use `cvar_get`); the marker's
  `Unknown command: cs2mcp_mark_...` line reaches console.log on both routes; both routes split on
  `;`. The engine's per-command limit is 510 bytes (live, 2026-10-26; the s2sdk header's
  `COMMAND_MAX_LENGTH` is 512 with a NUL and a margin), so a longer `;`-separated command is
  refused up front. Owed live (Live-fix C): a 510-byte single command runs, a 511-byte one is
  refused, and a 129-byte command name is refused.


## Where things are

This repo is its own project root (override with `--project-root` / `CS2_MCP_PROJECT_ROOT`):

```
<repo>/                     THIS repo = the default project root
├── bridge/                 pins.py/bridge_pins.h + build-rs/install-rs/runtime.sh; the bridge is crates/cs2-bridge (frozen C reference kept)
├── crates/cs2-mcp-common/  shared crate: bridge client, tool registry, rmcp adapter, runtime, sigs, capture, mock
├── hammer-mcp/             this server
├── client-mcp/             the game-client server (own AGENT.md)
└── workshop/               submodule: the workshop tools image source, published as ghcr.io/personcopperfine/cs2-workshop
    └── run/app             Windows CS2 + tools + Proton prefix (gitignored runtime state; mounted at /app)
```

Paths below are written from the repo root as `bridge/...`, `crates/cs2-mcp-common/...`,
`workshop/...`. Override with `--sigs` / `--project-root`.
Citations of the form `../idabins/...`, `../harness/...`, `../tests/...`, `../cs2-gym/...`,
`../.github/skills/...` point into the maintainer's private research checkout (the directory this
repo used to live in). They are not published; they record where a fact was established.

Required reading in idabins (skim; cite them, do not re-derive):

| Topic | Page |
|---|---|
| Page format, evidence tags `[V] [I] [?] [S] [D]` | `../idabins/docs/FORMAT.md` |
| Addresses, game update, bridge/runtime rules | `../idabins/docs/WORKFLOW.md` §9, §10, §11 |
| Bridge wire protocol, method table (§7), error codes (§8) | `../idabins/docs/data/bridge-protocol.md` |
| Signature DB format | `../idabins/docs/data/signatures-format.md` |
| Python POC (the contract we ported) | `../idabins/worknotes/mcpserver/README.md`, `server.py`, `bridge_client.py`, `mock_bridge.py` |
| Bridge DLL (this repo) | `bridge/README.md`, `crates/cs2-bridge/src/registry.rs` (the method table), `bridge/pins.py --rust` -> `crates/cs2-bridge/src/generated/pins.rs` (+ `bridge/bridge_pins.h`), `build-rs.sh`, `install-rs.sh`, `runtime.sh` |
| Hammer flows | `../idabins/docs/flows/hammer-engine.md`, `hammer-entity-edit.md`, `hammer-mesh-edit.md`, `hammer-mesh-operators.md`, `hammer-save-compile.md`, `hammer-undo.md`, `hammer-view-render.md`, `map-preview-freecam.md`, `file-edit-reload-compile.md` |
| Other tools | `modeldoc-edit.md`, `particles-edit.md`, `tool-plugin-admission.md`, `tools-frame-hook.md`, `tools-startup.md` |
| Wine runtime bring-up | `../idabins/worknotes/wine-tools-bringup.md` |

## Purpose and scope

In scope (priority order):
1. Hammer: the 24 POC tools (session, open/reload/save/run, entities, meshes, undo, renders) in Rust.
2. ModelDoc editor (`modeldoc_editor.dll`): `.vmdl` file route **done** plus the viewport render (11 `model_*` tools, bridge m2.11 `modeldoc.*` + m2.14 `modeldoc.render`, verified live); live node edits with undo are the second pass (`workflows/04-modeldoc.md`).
3. Particle editor (`pet.dll`): **done, both passes** — the `.vpcf` file route + live lifecycle + live
   node edits with in-editor undo + the preview render (18 `particle_*` tools, bridge m2.13 `pet.*`
   with 12 methods, verified live; `workflows/05-particles.md`).
4. Asset browser / asset editor: open assets by name, list/find, reveal in the right tool.
5. Vision + OCR: renders and screenshots as MCP image blocks, text extraction for dialogs/panels.
6. Headless: run the tools without the host X11 (today they need it; see workflow 08).
7. Low priority: SFM (`sfm.dll`).

Out of scope / deferred:
- **Workshop manager / item editor / publishing** — deferred (requires a real Steam login; the runtime
  uses the gbe emulator). Not planned; workflow 09 keeps only the SFM survey.
- Driving Qt UI (keypresses, clicks) as the primary mechanism. UI input is a last resort and only with
  the user's permission (WORKFLOW §11). The bridge calls Hammer's own functions instead.
- Reimplementing `resourcecompiler`, VPK packing or Steam upload.
- Patching/detouring Valve code. The bridge only vptr-swaps empty slots and calls documented entry points.
- Linux Hammer: there are no tools on Linux. The tools run as the Windows build under Proton/Wine.

## Architecture

```
MCP client (stdio) ──rmcp──▶ cs2-mcp-common::mcp     transport adapter; owns stdout; the only rmcp importer
                              │
                              ▼
                   cs2-mcp-common::tools              registry: name → schema, mutability, bridge methods, handler
                              │                       enforces capability gating, per-call budget, no-handle leak
          ┌───────────────────┼─────────────────────────┬──────────────────┐
          ▼                   ▼                         ▼                  ▼
   common::runtime       common::bridge            common::capture    hammer-mcp::vmap (file reads)
   Runtime trait         discovery → hello →        PNG from bridge    saved .vmap: entities,
   ├ docker (podman)     request (deadlineMs),      renders / game     meshes (stale-read warnings)
   └ local  (phase 2)    caps, modal gate,          screenshots / X    hammer-mcp::paths (Windows→host)
                         error taxonomy             grabs; ocr CLI     hammer-mcp::tools (24 tools, state)
          ▼                   ▼
   podman run/exec       crates/cs2-bridge cs2_bridge.dll inside cs2.exe -tools (Wine, container)   common::sigs: build pin
   vtmsend.exe           loopback TCP, NDJSON JSON-RPC, token from session.json          + doctor (no RVAs)
```

- **Shared crate.** `cs2-mcp-common` holds everything both servers need: `bridge` (client, discovery,
  errors, protocol), `tools` (registry, `ToolSpec`, `ToolError`, `Budget`, `ToolState`), `mcp` (rmcp
  adapter + `serve_stdio`), `status` (the bridge half of `session_status`), `runtime`, `sigs`,
  `capture`, `config` (`CommonConfig`, flattened into each binary's CLI) and, behind the `mock`
  feature, the Rust mock bridge (`mock::MockBridge`) + a stdio test driver (`mock::stdio`). This crate
  owns only Hammer-specific code: `tools/` (the 24 tools), `state` (unsaved-edit bookkeeping and the
  write gate), `geometry` (pure polygon geometry, the port of the POC's `geometry.py`),
  `paths` (Windows->host map), `vmap` (saved-map reads).
- **Runtime backend first, game second.** Tools never shell out to podman; they ask `runtime::Runtime`
  (`status/up/down/exec/paths/logs`). `docker` is the only backend in phase 1 (facts from
  `bridge/runtime.sh`: the client image (`--image` / `CS2_MCP_IMAGE`, default
  `ghcr.io/personcopperfine/cs2-docker:latest`), `/app` + `/cache` mounts, `--network=host`,
  `+engine_no_focus_sleep 0 +r_always_render_all_windows 1`, host X display). `local` comes later with the same trait (workflow 10).
- **Attach-only by default.** `--launch` opts into `runtime.up()` when the bridge is unreachable
  (not implemented until workflow 01; the flag is accepted and warns). `session_status` reports the
  launch command (`bridge/runtime.sh up`) when the bridge is down.
- **Bridge load trigger** is not TCP: a `tool_request cs2_bridge` record posted into the VToolMessage
  channel (`vtmsend.exe`, `../idabins/worknotes/vtmsend/`) after the manifest entry from
  `bridge/install-rs.sh` (`game/bin/sdkenginetools.txt`). The runtime backend owns that step.
- **Reads vs writes.** Live edits go over the bridge; reads of map contents come from the **saved**
  `.vmap` (file route) and say so (`staleRead`). Verification is save + parse, never the viewport.

### Relation to the POC and the bridge

The Python POC (`../idabins/worknotes/mcpserver/`) is the **contract**: tool names, arguments,
policies, error payload shapes and the mock/fixture test approach are ported 1:1 unless a workflow
says otherwise. `bridge_client.py` is fully ported (`crates/cs2-mcp-common/src/bridge/client.rs`);
`mock_bridge.py` is ported (`crates/cs2-mcp-common/src/mock.rs`, same knobs); the 24 tool bodies are
workflow 03.

The bridge DLL source now lives in **this repo** (`bridge/`): new methods (ModelDoc, pet, asset
info, client-side reads) are added there, pinned from `../idabins/signatures` through `pins.py`, and
documented in idabins' `bridge-protocol.md` §7 (`bridge/README.md` "Adding a method"). This server
is a client of protocol 1 / bridge m2.8 and must keep working when the bridge gains methods
(capability gating) and refuse cleanly when it does not (`NotImplemented` + `data.missing`).

Bridge method status that matters here (bridge-protocol.md §7, m2.8): implemented —
`control.*`, `doc.active`, `edit.batch`, `entity.create`, `entity.setKeyValues`, `undo.undo`,
`undo.redo` (m2.17; bridge-only — no tool here calls it), `map.save`, `map.reload`, `map.open`
(m2.16), `view.render`, `world.render`, `node.list`, `node.delete`, `mesh.create`,
`mesh.setGeometry`, `mesh.setMaterial`, and (m3.12) `selection.get`/`selection.set` (`get_selection`/`set_selection`); registered but `-32011
NotImplemented` — `doc.list`, `file.open_asset` (superseded by `map.open` in m2.16: the VToolMessage
asset route is not an open/reload primitive), `map.run` (which nothing here uses any more: WP-R's
`run_map` takes the VToolMessage command channel, above), `job.status`,
`mesh.get`; not registered — `console.command` (the bridge method; the *engine
console* is reachable through the VToolMessage channel, which is what `run_map` uses and what
runtime.sh itself posts the bridge load with), `map.compile`. Every undoable write's reply carries the m2.17 `doc` state its dispatch read
(`bridge/protocol-diff-m2.17.md`); `undo.redo` exists in the bridge from m2.17 but has no MCP tool. m2.11 adds the six `modeldoc.*` methods
(`bridge/protocol-diff-m2.11.md`); m2.12 adds the seven `pet.*` methods
(`bridge/protocol-diff-m2.12.md`, live test `bridge/test_pet.py`); m2.13 adds the five
live-edit/render ones making twelve `pet.*` total (`bridge/protocol-diff-m2.13.md`, live tests
`test_pet_live.py` / `test_pet_render.py`; both workflow-05 passes shipped and verified live); m2.14 adds
`modeldoc.render` (`bridge/protocol-diff-m2.14.md`, live test `test_modeldoc_render_live.py`); m2.15 adds
`entity.get`/`entity.find` and `entity.setKeyValues` by nodeID (`bridge/protocol-diff-m2.15.md`, test
`test_entities.py`); m2.16 adds `map.open` (`bridge/protocol-diff-m2.16.md`); m2.17 adds the write
`doc` tail and `undo.redo` (`bridge/protocol-diff-m2.17.md`). The
remaining bridge work in `bridge/` is the asset-browser methods (workflow 06).

## Build, run, test

```sh
cd <repo>
cargo clippy --workspace --all-targets   # must be warning-free
cargo test  --workspace                  # common contract tests + server tests + stdio e2e (no game)
cargo build -p hammer-mcp
RUST_LOG=info cargo run -p hammer-mcp -- --runtime docker   # project root: this checkout
```

MCP client config (stdio):

```json
{ "mcpServers": { "cs2-hammer": { "command": "<repo>/target/debug/hammer-mcp",
  "args": ["--project-root", "<repo>"] } } }
```

Live session today (until workflow 01 lands): `bridge/runtime.sh up|open|status|logs`
(needs the host X display and `XAUTHORITY`; one CS2 at a time — ~13 GB RSS, measured on a host without swap).
ModelDoc: **one instance at a time**, started with `TOOL=modeldoc bridge/runtime.sh up`;
`runtime.sh` refuses a second one and refuses `.vmdl` opens elsewhere (workflow 04).
pet: runs in the default tools container (`cs2tools`, `TOOL=hammer`); `runtime.sh up` clears pet's
unclean-exit flags ("Recover Files?" modal after a kill) unless another tools container shares the
tree — one tools container at a time (workflow 05).

### Configuration (flags = env, `cs2-mcp-common::config::CommonConfig`)

`--runtime docker|local` (`CS2_MCP_RUNTIME`), `--discovery` (`CS2_MCP_DISCOVERY`), `--pfx` (`CS2_MCP_PFX`),
`--project-root` (`CS2_MCP_PROJECT_ROOT`, default: two levels above the common crate = this
checkout; a parent directory holding the repo as `cs2-mcp/` works too), `--image` (`CS2_MCP_IMAGE`,
default `ghcr.io/personcopperfine/cs2-docker:latest`), `--sigs` (`CS2_MCP_SIGS`, default `<repo>/sigs/signatures`), `--path-map`
(`CS2_MCP_PATH_MAP`), `--expect-build` (`CS2_MCP_EXPECT_BUILD`), `--deadline-ms 30000`,
`--tool-budget-ms 45000`, `--blocked-age-ms 10000`, `--no-blocked-gate`, `--launch`.

## Conventions (binding)

1. **No addresses in Rust.** Never write an RVA, vtable slot number or struct offset in any crate of
   this repo. Addresses live in `../idabins/signatures/<module>.json` and reach the game only through
   the bridge DLL (`bridge/pins.py` -> `bridge_pins.h`, fail-closed, `-32017 PrerequisiteFailed`).
   `common::sigs` reads the DB for build pinning and the `doctor` report, by **entry name** only.
   Need a new symbol? Signature entry (WORKFLOW §9) + bridge method in `bridge/`, then consume the
   method here.
2. **stdout is the protocol.** All logs via `tracing` to stderr (`RUST_LOG`). Never `println!`.
   The stdio e2e test asserts every stdout line is a JSON-RPC frame.
3. **Error taxonomy** (`common::bridge::errors`, `common::tools::ToolError`; the POC table): every tool
   error is `{type, name, code, wireName, message, data, retrySafe, outcomeUnknown}`.
   **Switch on `type`/`name`, never on `code`.** `code` is a JSON-RPC-style code when one applies:
   errors from the bridge carry its wire code (`-32602`, `-32011`, ...), many host-side argument and
   path refusals reuse `-32602` (`WrongEngineAddon` among them), and many other host-side refusals
   (`CompileRunning`, `BridgeBusy`, `OpenUnconfirmed`, `OpenPending`, ...) have `code: null`. It is
   not unique per type.
   `retrySafe:false` (`OutcomeUnknown`, `ConnectionLost`, `PartialFailure`, `NotCancellable`, unknown
   codes) means *verify, do not retry*. Wire codes -32001..-32022 map 1:1.
4. **Deadlines.** Explicit `deadlineMs` in `params` on every bridge request (never 0), derived from
   the per-tool-call `Budget` (`min(remaining budget, --deadline-ms)`; exhaustion =
   `ToolBudgetExceeded`, retry-safe). Timeout classification exactly as the POC: `-32008` ->
   `QueueTimeout` (safe), no reply with socket alive -> `OutcomeUnknown`, socket closed -> `ConnectionLost`.
5. **Modal gate, fail closed.** `control.ping` before any non-`control.*` method; refuse with
   `HammerBlocked` when `mainThreadAgeMs` is stale (> `--blocked-age-ms`) or unverifiable; a job we
   started (`note_job` — kept for bridge jobs a `control.status` entry can confirm) is `EngineBusy`
   instead, and a job the **bridge knows nothing about** (`note_external_job`: `run_map`'s console
   load) is `EngineBusy` until its own grace expires, since `control.status` lists no `running` job
   for it (review-1 L1). The two graces live in SEPARATE fields (`job_grace_until` /
   `external_grace_until`, review-2 N4): `clear_external_job` releases only the external window,
   and the `EngineBusy` gate's data says whether the excuse IS the external one
   (`externalJob` + `externalGraceRemainingMs`). `mainThreadAgeMs >= 2^63` reads as 0
   (old-bridge race).
6. **Capability gating** is the registry's job: tools declare `bridge_methods`; if any is not
   advertised (after one `control.capabilities` refresh) the tool returns `NotImplemented{data.missing}`
   without sending anything; a dead bridge is `BridgeNotRunning`, never a missing capability.
   Capabilities are also re-fetched when `control.ping.revision` changes.
7. **Safety policies.** No auto-save (`save_map` is the only `Mutability::Writer`; mutations bump the
   unsaved-edits counter that file reads report as `staleRead`). `reload_map` refuses while `dirty` is
   true **or unknown** unless `force`. `open_map` is not a dirty gate: it *adds* a session (the current
   one keeps its unsaved state), so it only refuses an already-open file — which the bridge answers as
   `existing:true` (Hammer's own session match is a stub, so calling `OpenFile` again would create a
   second session for the same file and leak the first). `undo_last` refuses a group not created by this
   server unless `acceptUserEdits`. Never exposed (the registry panics if a tool declares them):
   `console.command`, `undo.redo`, `material.assign`, `entity.delete`, `control.shutdown`,
   `game.saveRestore` (`job.cancel` left the list with the bridge job model: see "Jobs";
   `selection.*` with the live selection tools: see "Selection"); raw `mesh.*` handles never leave the server (`strip_handles`
   runs on every result and error payload); handles are only valid inside the `edit.batch` that
   produced them (`"$<opIndex>"`).
8. **One client at a time**, one bridge connection (`SharedBridge` = async mutex); reconnect re-reads
   discovery (token rotates).
9. **Windows -> host paths** (`hammer_mcp::paths::PathMap`) are mapped by longest prefix then
   resolved case-insensitively; anything unmapped is `PathNotMapped`, never guessed.
10. **Evidence discipline.** Claims in docs carry FORMAT.md tags. Bridge/server behaviour is built on
    `[V]` claims; `[I]`/`[?]` items are upgraded first (runtime check) and listed under Open questions.
11. **Ask before driving the desktop** (keypresses, focus). Prefer the bridge; UI equivalence checks
    (Ctrl+Z, Ctrl+S) are opt-in.
12. **Game update** = idabins WORKFLOW §10 + `bridge/pins.py --rust`/`bridge/build-rs.sh`. This crate's only reaction is
    `BuildMismatch` until the sig DB records the new build and the bridge is rebuilt; `session_status`
    /`doctor` must say exactly that.
13. **Agent-readable results (`cs2_mcp_common::tools`, WP2.1).** `tools/list` carries MCP annotations
    from `Mutability` (ReadOnly -> `readOnlyHint`; Mutating -> not read-only, not destructive: live,
    undoable, unsaved; Writer -> destructive; `openWorldHint:false` always; `ToolSpec::idempotent(bool)`
    overrides per tool). A result may carry `next:[{tool, why, args?}]` (`ToolOutput::with_next`,
    `ToolSpec::then`, or `Registry::suggest_after`; a handler's own list wins, an error never has one):
    only real next steps, kept short. Hammer wires the iterate loop in `tools::wire_next_steps`
    (`LIVE_MUTATORS` -> `save_map` -> `compile_map` -> poll -> `run_map {path}` -> `game_state`/`render_world`);
    a mutator's `save_map` step is decided per call (`mutator_next`: only with unsaved edits, a titled
    document and a real change; `ToolOutput::no_next` suppresses the registration-time step). A new
    live mutator that calls `note_edit` goes in `LIVE_MUTATORS`; every other `Mutating` tool goes in
    `NOT_DOCUMENT_EDITS` (a registry test enforces the partition). `ToolSpec::destructive(true)` marks
    tools that discard unsaved work (`reload_map`, `model_reload`, `particle_reload`). Errors keep the exact shape; the
    two standard optional `data` fields are `didYouMean:[names]` (`ToolError::did_you_mean`, omitted
    when empty; the old `suggestions` field is gone) and `useInstead:{tool, why, args?}`
    (`ToolError::use_instead`); an unknown tool name is the JSON-RPC `-32602` with
    `data:{type:"UnknownTool", didYouMean?}` (omitted when nothing is close). A bare `hint` stays only where it states a fact.
    **Aliases:** a renamed/merged tool keeps its old name for one release with
    `Registry::alias(old, new, fixed_args)` (listed with a one-line description and an open schema,
    same annotations; fixed args win over the caller's; the result gains `deprecated:{alias,use,args?}` and its description names the fixed args; an error
    from an alias call carries `data.useInstead` with the new name and fixed args; gated by the
    target's capabilities).

## Layout

```
hammer-mcp/
├── AGENT.md, README.md, Cargo.toml       lib (hammer_mcp) + bin (hammer-mcp)
├── src/
│   ├── lib.rs             build_server(): registry + state + identity
│   ├── main.rs            clap (CommonConfig flattened) → init_tracing → serve_stdio
│   ├── state.rs           HammerState: SharedBridge, PathMap, unsaved-edit counter, write gate, launch command
│   ├── geometry.rs        pure polygon-mesh geometry (port of the POC's geometry.py) + unit tests
│   ├── tools/             hammer.rs (24 tools, POC order; 1 documented stub), session.rs (session_status), render.rs (renders), run_map.rs (the engine load),
│   │                      live_common.rs (the plumbing every live mutator shares: document read,
│   │                      expectDoc, unsaved-edit counter, maybe-executed taxonomy; review L7),
│   │                      mesh_file.rs (list_meshes/get_mesh),
│   │                      mesh_live.rs (create_mesh/create_block/set_mesh_geometry/set_mesh_material
│   │                      + the rebuild operators transform_mesh/move_mesh_vertices/
│   │                      extrude_mesh_face/delete_mesh_faces)
│   ├── paths/             Windows→host mapping (+ unit tests)
│   ├── resource/          compiled Source 2 resources: the container + binary KV3 + the compiled-model facts library (WP-M, `get_model_info`)
│   └── vmap/              saved-.vmap reads (reader choice open; workflow 03)
├── tests/                 server.rs (mock-backed contract), stdio.rs (real binary over stdio), resource.rs (the compiled-resource library + its corpus tests)
├── fixtures/              README only — the recording lives in crates/cs2-mcp-common/fixtures/
├── research/              dev tooling in any language (Python ok)
├── docker/                our own image layer: Dockerfile + entrypoint stubs
└── workflows/             00..10, numbered, actionable
```

## Workflows

| # | File | Outcome |
|---|---|---|
| 00 | `workflows/00-bootstrap.md` | **done**: rmcp stdio server, bridge client, mock bridge, `session_status`, tests |
| 01 | `workflows/01-docker-runtime.md` | `docker` backend: up/down/exec/logs, bridge load, our image layer |
| 02 | `workflows/02-sig-resolution.md` | sig DB consumption, build pinning, `doctor`, update drill |
| 03 | `workflows/03-hammer-tools.md` | entities **done** (m2.15); lifecycle: `save_map`/`reload_map`/`open_map` (m2.16) **done**, `run_map` **done** (WP-R), `compile_map` **done** (WP-C); mesh writes **done** (`geometry.rs` port + `create_mesh`/`create_block`/`set_mesh_geometry`/`set_mesh_material`/`transform_mesh`/`move_mesh_vertices`/`extrude_mesh_face`/`delete_mesh_faces`, m2.5 methods), mesh file reads **done** (WP-A), renders **done** (WP-D); triggers **done** (5 tools, bridge m2.19, verified live — `## Triggers`); `.vmap` reader = Rust port (b) |
| 04 | `workflows/04-modeldoc.md` | **first pass + vision done**: ModelDoc file route (edit on disk, reload, compile) + bridge m2.11, `model_render` (bridge m2.14, verified live); live node edits deferred |
| 05 | `workflows/05-particles.md` | **done (both passes)**: pet `.vpcf` file route + live lifecycle (13 tools, bridge m2.12 `pet.*`, schema snapshot, crash rules) and the second pass (18 tools, bridge m2.13 `pet.*` 12 methods: live edit with undo, `pet.text`, `pet.render` vision; verified live) |
| 06 | `workflows/06-asset-browser.md` | asset open/list/find via framework + AssetSystem |
| 07 | `workflows/07-vision-ocr.md` | image blocks, OCR, dialog detection |
| 08 | `workflows/08-headless.md` | tools without the host X11 |
| 09 | `workflows/09-sfm-workshop.md` | SFM survey (low priority); workshop publishing deferred/out of scope |
| 10 | `workflows/10-local-install.md` | `local` runtime backend |

`particle_compile` takes a file or a folder of the active addon (`recursive:true` descends) and, with
`children` (default true), also the `m_Children[].m_ChildRef` files they reference (each once, cycles
guarded, children first). More than one file is one `resourcecompiler` run with several `-i` (bridge
route: one job per file) and returns `files:[{path, ok, upToDate?, failLines, referencedBy?}]`; `ok` is
true only if every file is. Each `m_ChildRef` resolves in order, first hit wins: addon source (compiled
with the set; matched case-insensitively), addon compiled `_c` without a source (`source:"addon-compiled"`),
a source under `content/csgo` or `content/core`, a `_c` entry in `game/csgo|core/pak01_dir.vpk`
(`vpk:csgo|vpk:core`, through the model catalog's cached trees). The last three are `external:true`,
`ok:true`, not compiled, and never count against the overall `ok` (if the VPKs cannot be read at all, such a child is `source:"unknown", verified:false` plus a note, not a failure). A child found nowhere is a per-file
failure naming its parent (never sent to the compiler). On the podman route a clean run must report
compiled + skipped of AT LEAST the inputs sent (the compiler also counts dependencies; else `ok:false`, `reason`), and an up-to-date output only
counts then. A lone file with no children keeps the single-file result (plus `files`). Symlinks met by
a folder walk are skipped and listed in `notes`. `compile_map` results also carry `stageFlags` (what was
sent) and `stagesFrom` (`explicit`, `preset`, `compiler default`).

## align_texture (bridge m3.14, `src/tools/mesh_live.rs`)

`align_texture {nodeID, mode, faces?, justify?, keepScale?, shift?, scale?, rotate?, texSet?}` (Mutating,
`mesh.setFaceTexture`) sets the UV mapping of a LIVE mesh's faces: `mode` `world` snaps the texture to the
world grid (offset 0, default scale; the ti_convoy floor-letter case), `face` aligns it to each face's plane,
`justify` fits it (`justify` left/right/top/bottom/center/fit/fitU/fitV; shift/scale/rotate are refused with it, they would undo it), `set` applies only the explicit
fields. `shift` is in texels, `scale` in world units per texel (> 0), `rotate` in degrees (sign unverified, a live
check; it pivots at texture (0,0), so the texture also slides). The fields are applied after the mode,
per face, in one undo entry. `faces` are `get_mesh` indices (default every face).

- Capability-gated like every bridge-backed tool: a bridge without the method answers
  `NotImplemented{data.missing:["mesh.setFaceTexture"]}` and sends nothing.
- Argument shape errors (enums, scale <= 0, non-finite numbers, duplicate or negative indices) are refused
  locally. An index past the mesh's face count is refused by the bridge before it writes anything (`-32602`,
  retry-safe, no undo entry, no unsaved-counter bump): the tool does not read the mesh to count its faces.
- Success bumps the unsaved counter and answers `next:"save_map"` plus the per-face read-back
  (`faces:[{index, axisU:[x,y,z,w], axisV, scale}]`, first 1000 faces, `readBackTruncated`). `get_mesh` does
  not report the texture fields yet (`mesh.get` has no `texture` member).
- **`mode:"axes"` (bridge m3.18, I3)**: `faces:[{index, axisU:[x,y,z,offset], axisV:[x,y,z,offset],
  scale:[su,sv]}]` writes ABSOLUTE axes per face (`texSet` ok; `shift`/`scale`/`rotate`/`justify`/`keepScale`
  are refused with it). Texel of a world point `p`: `dot(p, axisU.xyz)/su + axisU.w` (offset in texels, scale in
  world units per texel, axes need not be unit). Validated locally like the bridge does: finite numbers, scale > 0,
  axis xyz not zero-length and not parallel, unique indices (<= 65 536 faces, request under 512 KB), so nothing bad
  is sent; an older bridge answers its own `-32602` for the mode. The reply is the usual read-back plus
  `exact:true|false` (every face read back bit-for-bit what was sent; false = Hammer normalised something, compare
  `faces`). One undo entry per call.

## Live-fix A (compile / particle paths / open_map, bridge m3.16)

- **Bridge m3.16** (`bridge/protocol-diff-m3.16.md`): `job.start` takes `-world -phys -vis
  -bakelighting` (plus the old `-nav`...). `cs2_bridge::ops::job::{PLAIN_FLAGS, NUMERIC_FLAGS}` are
  public; hammer-mcp dev-depends on cs2-bridge and `compile_map.rs`
  `bridge_flags_are_all_in_the_job_allowlist` runs every mode and every stage subset through the
  bridge's own `parse_flags`. A new flag hammer-mcp emits fails that test until the allowlist has
  it. Against an older bridge `novis`/`stages` still fail with `flag "-world" is not allowed`.
- **Paths** (`vmdl/paths.rs` `candidate()`, shared by `particle_*` and `model_*`): a relative path
  is addon-relative (`particles/x.vpcf`, `models/x.vmdl`) unless its first segment is
  `csgo_addons`, `core` or `csgo` (content-relative). The "outside the addon" error lists the
  accepted forms with an example.
- **`particle_compile`**: `force:true` adds `-f` (both routes). A compiler skip is explained:
  results carry `reason`, `skippedByCompiler` (the `skipped` count stays a number), and every
  `files[]` verdict (including not-attempted ones) has `skippedByCompiler` (bool) and `reason`. A skip over an output older than the source
  is `ok:false` with a reason that suggests `force:true`; an up-to-date skip stays `ok:true`
  (`upToDate:true`). A not-ok single result with no `failLines` always has a `reason`.
  `model_compile` and `compile_soundevents` get the same `reason`/`skippedByCompiler`, but have no
  force option (the reason says to touch the source).
- **`open_map`** prechecks the file under the ENGINE's addon, because the bridge's `map.open`
  resolves `maps/...` there. That addon is the Hammer tools container's `cs2mcp.addon` label
  (`data.engineAddonSource:"container"`), falling back to `--addon` (`"flag"`); a `notes` entry
  says when the engine's copy was opened although another addon is selected. A map that exists only in the `set_addon` addon is
  refused with `WrongEngineAddon` (`data.engineAddon`, `data.selectedAddon`) naming the runtime
  `ADDON=<name>` route. See FIXME "Live-fix A".
  **First map of a fresh session (Live-fix C):** hammer.dll is not loaded until a document opens,
  so the bridge does not advertise `map.open` yet. `open_map` no longer has a registry capability
  gate: if `map.open` is missing and a tools container is running, it posts the VToolMessage
  `open_asset` (`_data = "maps/<x>.vmap"`, the canonical addon-relative form; never the
  `csgo_addons/<addon>/...` form, which raises a blocking "Unrecognized Asset" modal) under the
  `vtm_gate`, then polls `refresh_capabilities` + `doc.active` within the call budget until the
  document is that map, and answers `openedVia:"vtm"` (with `post`). The same prechecks run first
  (path shape, engine addon, file exists, local vmap parse). If the document does not show up in
  the budget the error is `OpenUnconfirmed` (outcome unknown, not retry-safe: a modal such as
  "Error opening file" may be up; call `session_status`, do not post another open blind). With no
  tools container to post through the old `NotImplemented` (`missing:["map.open"]`) stays, with
  `data.hint` naming `bridge/runtime.sh open`. Later opens use `map.open`.
  The vtm route is taken ONLY while the bridge reports `modules["hammer.dll"].state` as
  `not_loaded`/`verifying` (a loaded Hammer without `map.open` keeps `NotImplemented`: `open_asset`
  bypasses the session-leak guard). Before hammer.dll loads, the bridge answers `doc.active` with
  `-32017 PrerequisiteFailed {module:"hammer.dll", state:"not_loaded"}`; `BridgeClient::active_document`
  maps that to `HammerNotLoaded`, so the pre-call addon check lets the call through. Under the
  `vtm_gate` the route first settles this server's own unresolved `run_map` load (`EngineBusy`,
  nothing sent), refuses a second post while a first one is younger than 60 s with no document
  (`OpenPending`, retry-safe, nothing sent), and needs at least 5 s of call budget (`BudgetTooLow`,
  retry-safe); the gate wait and each probe are bounded by the budget (`BridgeBusy`).
  **Map already open in a background tab (Live-fix C):** `existing:true, active:false` now says
  so plainly in `note` and in `activeDocument` (the path the tools act on). No tool switches or
  closes Hammer tabs; the note offers what exists: read the saved file by path
  (`list_entities`/`find_entities`/`get_entity`/`list_meshes`/`get_mesh`), or ask the user to
  switch tabs, then `session_status`. A real tab switch is bridge work (FIXME "Live-fix C").
- **`run_map`**: refused (`CompileRunning`, `data.jobId`, `useInstead` -> `job_status`) while a
  `compile_map` job for the same map is running; `ignoreRunningCompile:true` overrides. It also
  reads the vpk's 12-byte header and refuses a file that is not a VPK (`InvalidVpk`, `useInstead`
  -> `compile_map`), since a corrupt vpk FATALs the engine.
- `particle_create` returns `next` steps (`particle_open`, `particle_compile` with the path) in
  place of the string `nextStep`.

## new_map (`src/tools/new_map.rs`, streamline acceptance blocker)

`new_map {path:"maps/<name>.vmap", template?, open?:true, overwrite?:false}` is the only tool that
creates a `.vmap`. In an empty addon nothing else works: `open_map` needs an existing file and the
edit tools need a loaded Hammer, which loads with the first document.

- **Template.** Default: Valve's `<project-root>/workshop/run/app/content/csgo_addons/addon_template/maps/xxx_mapname_xxx.vmap`
  (else the first `.vmap` in that directory). Or `template:"maps/<x>.vmap"`, an existing map of the
  SELECTED addon. The template is parsed with `crate::vmap` first (`InvalidMapFile`, nothing written)
  and copied byte for byte. `template:"empty"` is refused: not offered (FIXME "new_map").
- **`clear: "gameplay"|"all"|"none"` (live-fix D, default none).** After the open, the template's
  nodes are deleted live through `node.delete` (`lifecycle::delete_nodes_locked`, the body of
  `delete_nodes`, called under `new_map`'s own gate). The ids are the parsed template's `nodeID`s
  (a freshly opened copy has the file's ids): meshes plus `CMapEntity` records, the world never.
  `gameplay` removes every mesh and entities whose class starts with `info_player_`, `func_`,
  `trigger_`, `prop_`, `logic_`, `game_`, `point_`, `weapon_`, `item_`, `hostage`, `info_target`; it
  KEEPS `ENVIRONMENT_CLASSES` (sky, `light_environment`, fog, cubemap, post-processing, map
  parameters, soundscapes, light probes, `cs_minimap_boundary`) and anything unknown, and also the
  match-flow cameras (`team_select`, `end_of_match`, the team intros: `defuse_poc`, a map that ran,
  keeps them). `all` deletes every entity and mesh (worldspawn and node kinds the plan cannot delete, such as the
  template's nav data or a group, stay and are listed under `kept`); `gameplay`'s "every mesh"
  includes the template's `toolsskybox` mesh. Before the first chunk the ACTIVE document is read and
  compared (path, epoch) with the open's; a mismatch skips the clear (`clear.skipped`, nothing
  deleted). The reply's `clear {mode, planned, removed {total, byClass} (the chunks that
  succeeded), kept {total, byClass}, deleted, undoGroups, skipped?, note}`; one request (<= 256 ids) is one undo group,
  so the real template (about 76 removable nodes) is one `undo_last`. A delete failure is reported
  inside `clear.error` (the file exists and is open); `open:false` with `clear` is refused before
  anything is written; an `existing` (already-open) document skips the clear. `next` stays
  `apply_layout`, `save_map`. The class lists were checked against the real 2000933 template:
  `point_worldtext`, `info_player_*`, `func_buyzone`, `func_bomb_target`, `point_nav_walkable` and
  the meshes go; `env_sky`, `light_environment`, `env_cubemap_fog`, `post_processing_volume`,
  `info_map_parameters`, `env_soundscape`, `env_combined_light_probe_volume`,
  `cs_minimap_boundary`, `team_select`, `end_of_match`, `*_team_intro` stay
  (`new_map` test `the_class_lists_fit_the_real_templates_contents` fails on an unclassified class).
- **What the real template holds** (build 2000933 install, read with `cs2-dmx`, 228033 bytes,
  binary 9 / vmap 35): 70 entities by the reader (69 `CMapEntity` + `worldspawn`): 16
  `info_player_counterterrorist`, 16 `info_player_terrorist`, 1 `info_player_start`, 2
  `func_bomb_target`, 2 `func_buyzone`, `info_map_parameters`, `env_sky`, `light_environment`,
  `env_cubemap_fog`, 2 `env_combined_light_probe_volume`, `post_processing_volume`, 2
  `env_soundscape`, 2 `cs_minimap_boundary`, `point_nav_walkable`, 16 `point_worldtext` labels, team
  intro/select/end-of-match entities; 23 `CMapMesh`, 17 node instances, nav data, a selection set,
  cameras. A Hammer-saved file: `cs2-dmx` reads it and writes it back byte for byte.
  The result's `contents` reports the same shape for whatever template was used
  (`entities`, `meshes`, `spawns`, `byClass`, `bytes`).
- **Writes** only inside the selected addon's `maps/` (the `open_map` shape gate, plus a symlink check
  on the real path: `PathOutsideAddon`), refuses an existing target (`MapExists`, `useInstead`
  `open_map`) unless `overwrite:true` (the old file is kept as `<name>.vmap.bak-<seconds>[-k]`), and writes
  atomically (`vmdl::fsutil::write_atomic_bytes_with_backup`: temporary file + rename; without
  `overwrite` `write_atomic_bytes_new` hard-links the temporary into place, so a file made meanwhile is
  never replaced). Also refused: a path differing only in case from an existing file or directory
  (`MapExists`, `didYouMean` the real spelling: this host is case-sensitive, Hammer is not), an
  `overwrite` of the document open in Hammer (`MapOpenInHammer`), a pending first-map open
  (`OpenPending`, before anything is written), and the selected addon `addon_template` (never written).
  If `overwrite` hits a map already open in a background tab, the result has `existing:true`,
  `editorStale:true` and no `save_map` step (`reload_map {force}` only when it is the active tab).
- **Template assets** (`new_map_assets.rs`, default template only): the references the map makes to
  the template ADDON are copied without replacing anything, never touching `addon_template`:
  `postprocess/basic_linear_post.vpost` into `content/` (its compiled `_c` into `game/csgo_addons/<addon>/`
  when the template addon has one and that dir exists), and `soundevents/soundevents_addon.vsndevts`
  only when the addon has none. Result: `templateAssets {copied, missing, alreadyPresent}`; `missing`
  carries the reason (a `.vpost` not in the template addon, an existing soundevents file left alone,
  the unreferenced `.vsnd` tracks). Beyond the thumbnail and `map_asset_references` (rewritten by
  Hammer's first save) that is what makes the copy self-contained.
- **Opens** (default) by calling `lifecycle::open_map_locked` under the same write gate: the first-map
  vtm route, the engine-addon check (`WrongEngineAddon` is raised BEFORE the file is written; pass
  `open:false` to create in a non-engine addon), the local parse and the document view are
  `open_map`'s. The result is open_map's plus `created`, `template`, `contents`; `next` is
  `apply_layout`, `save_map`. If the open fails the error carries `created:true`, `createdPath`, and
  (unless the outcome is unknown) `useInstead open_map`.
- **Registration.** `NOT_DOCUMENT_EDITS` (it creates a document, it does not edit one: no
  `note_edit`), NOT in `ADDON_EXEMPT_TOOLS` (it needs the pre-call addon check; with Hammer not
  loaded `doc.active`'s `HammerNotLoaded` lets it through), no bridge methods. Registered last
  (`NEW_MAP_TOOL_ORDER`).
- **Pointers at it.** `session_status` lists `maps` (names only, `maps/...vmap`, capped at 20, `total`)
  and, with no document, `next` is `open_map` (the first map) when the addon has maps, else `new_map`.
  `open_map` (`missing_host_file`), `compile_map` (missing `.vmap`) and `run_map` (`VpkNotFound` with
  no `.vmap` either) carry `useInstead new_map {path}` only when the file exists in no spelling; a
  case-only mismatch (`maps/Foo.vmap` asked, `maps/foo.vmap` on disk) answers `didYouMean` instead.
  `session_status` shows `pendingOpen {path, ageMs}` with `next session_status` while a first-map open
  is loading, and `next set_addon {engine addon}` when the selected addon is not the engine's.

## WP2.3: live/saved twins merged, `get_scene_info`

- **`get_entity` / `find_entities` take `source: "live"|"saved"`** (`entities_read.rs`; the saved
  handlers stay in `entities_file.rs`, the live ones in `entities_query.rs`). Both are registered with
  no `bridge_methods` (the saved route needs no bridge), so the live route does its own capability
  check (`need_live`: `entity.get` / `entity.find`, `CapabilityMissing` as before).
- **Default** (`source` omitted): live when no saved-only argument is given AND `doc.active` answers AND
  the bridge advertises the methods (`entity.get`, plus `entity.find` for a `get_entity {targetname}`);
  else saved, with `sourceWhy` saying which condition decided (it names the open document, and tells a
  silent `doc.active` from "no document open"). A live default stamps `document: <path>`. The
  saved-only arguments that switch the route are `path` (and, on `find_entities`, `withKeyvalues`).
  **`nameContains` does not switch it**: `entity.find` has no substring filter, so on the live default
  it is refused with a pointer to `get_scene_info {query}` (it still works with `source:"saved"`). **This changed the no-`path` default from saved to live**:
  tests that read the saved file of the active document now say `source:"saved"`.
- Every result carries `sourceUsed`. Output fields are untouched per route (`entity`/`entities` + the
  `source` object and `staleRead` on saved; the bridge reply, `source:"live"` and the I/O `note` on
  live), so old callers parse the same shapes.
- **Refused, nothing sent** (InvalidParams naming the argument and the source that takes it): `path`
  on live; `nameContains`/`withKeyvalues` on live. `get_entity` live accepts `targetname` by mapping it
  through `entity.find` (limit 2) to a nodeID.
- **`targetname` rule, both routes:** case-insensitive (Hammer's own targetname semantics; saved used to
  be case-sensitive), and two or more matches are `AmbiguousTargetname` (retry-safe,
  `data.candidates:[{nodeID,classname,targetname}]`, "pass nodeID") instead of the first hit.
- **`sourceUsed` is the stable discriminator.** The legacy per-route fields are kept (`source` is the
  string `"live"` on a live read and a `{fromDisk,hostPath,...}` object on a saved one), but
  `get_scene_info`, which has no legacy shape, never emits `source`: the saved file's data is under
  `file:{hostPath,mtime}`.
- `find_entities` saved still needs at least one filter; live with none lists up to `limit`.
- **Aliases:** `get_entity_live` -> `get_entity {source:"live"}`, `find_entities_live` ->
  `find_entities {source:"live"}` (fixed args win, so an alias never reads the file). Registered after
  the entity tools, inside `ENTITY_TOOL_ORDER`.
- **Aliases stay listed (last) for one release**, as `Registry::alias` documents: an MCP client only
  exposes what `tools/list` returns, so hiding them would make them uncallable for agents. Each has a
  one-line description (`Deprecated: use `get_entity` {"source":"live"}.`) and an open schema. Remove
  them in the following release.
- **`get_scene_info {query?, fields?, limit?, source?, path?}`** (`scene_info.rs`, ReadOnly, in
  `ADDON_EXEMPT_TOOLS` because both routes read `doc.active` themselves). One read: `counts`
  (entities, distinct classes, meshes), `classes` (an ARRAY of `[classname,count]`, biggest first, at
  most 40, then `otherClasses:N`), `bounds` (entity origins + mesh extents saved / mesh origins live;
  a saved mesh with no vertices is left out), `spawns` (`info_player_*`, `info_deathmatch_spawn`, at
  most 40, `spawnsTruncated`), and `items`: the first `limit` (default 25, at most 500)
  entities whose classname or targetname contains `query`, with `fields` from
  `nodeID|classname|targetname|origin`. `matched` counts every hit; `truncated` is `matched > items`.
  Live: `entity.find` (2000 max, a note says so when the document has more) + `node.list class:mesh`
  (limit 4000, under the bridge's 512 KB result cap) for the mesh count when advertised. A missing
  `node.list`, a failed call or a truncated page gives `counts.meshes:null` and says why in `note`.
- **Decisions.** `list_entities` stays (its `classnames` histogram and full unfiltered list on the saved
  route; folding it into `find_entities` would need the no-filter relaxation on saved and an alias that
  loses `classnames`). `list_meshes`/`list_props`/`list_triggers`/`list_models`/`list_soundevents` stay:
  they carry per-item detail (bounds, models, volumes, catalogs) `get_scene_info` leaves out;
  `get_scene_info` covers entities (so props/triggers/sounds show up as classes) and meshes only.

## prefab (I4, `src/tools/prefab.rs`, `src/vmap/prefab.rs`; bridge m3.18)

`prefab {action}` (Mutating; `create|instance|list|read`) manages reusable `.vmap` files under
`maps/prefabs/` of the active addon. CS2 has no `func_instance`: an instance is a `CMapPrefab` node
(`targetMapPath` + origin/angles/scales) that the bridge adds with `node.createPrefab`. Gated per
action inside the handler (`need_live`): `create` needs `selection.set` + `node.prefabFromSelection`,
`instance` needs `node.createPrefab` (else `CapabilityMissing`/`NotImplemented`, `data.missing`); `list`
and `read` are file-only. Every call holds the server write gate.

- `create {name, nodeIDs, centered?}`: `name` is `[a-z0-9_-]{1,64}` (no separators, dots, capitals) and
  becomes `maps/prefabs/<name>.vmap`; an existing file is `PrefabExists` before any bridge write.
  The tool makes `maps/prefabs/` (the bridge refuses a missing directory), reads `selection.get`
  (reported as `previousSelection`, not restored: the originals are gone afterwards), `selection.set
  {op:"replace", nodeIDs}` (if Hammer left any id out: `SelectionNotApplied`, `notSelected`, no prefab),
  then `node.prefabFromSelection {path, centered}`. **Hammer writes the file at once and REPLACES the
  selected nodes in the open document with the prefab node** (the PLAN said "document untouched"; the
  bridge worker does not work that way). `undo_last` restores the originals, the file stays on disk
  (`fileStaysOnUndo`). `centered:true` re-centres the file on the selection bounds and puts the node at
  that centre; `false` keeps the world offset (the node sits at the origin). The open map is not saved.
- `instance {name|path, origin, angles?}`: `path` is an addon-relative `maps/....vmap`; a missing file is
  `PrefabNotFound{available}` (checked on disk first). `node.createPrefab` refuses the map that is open
  in Hammer (`-32602`). `loaded:false` in the reply becomes a `WARNING` in `note`. Instance the same file
  twice for two copies. Chain: `look {target:nodeID}`, `save_map`, `compile_map`, `run_map`
  (`run_map {unload:true}` first when the map is loaded in the engine).
- `list`: the `.vmap` files under `maps/prefabs/**` with entity/mesh/prefab counts (first 50 files).
  `read {name|path, limit?}`: entities, mesh summaries and nested prefab nodes through `src/vmap`.
- `src/vmap/prefab.rs`: `Document::prefabs()` lists the `CMapPrefab` nodes (`PrefabRecord`: nodeID,
  `targetMapPath`, origin/angles/scales, the optional flags the file carries). A prefab is not an
  entity (the entity view and its goldens are unchanged) and not a mesh parent (`mesh::parent_problem`
  keeps refusing a mesh under it). `list_entities` adds `prefabCount` + `prefabs`, `get_scene_info` (saved)
  `counts.prefabs` + `prefabs`. The live `get_scene_info` route does not count prefabs (see FIXME).
- Tests: `tests/prefab.rs` (reader fixture built with `cs2_dmx`, create/instance/list/read/gating/name
  validation on the mock bridge). Open items and inferences: `workflows/import/FIXME.md` `## I4`.

## look (WP3.1, `src/tools/look.rs`)

One tool to see the map, built on the render primitives (`render_view` / `render_world` stay,
unchanged; `look` calls their shared pipeline `render::run` directly, so it checks the render
capability itself). Read-only and render-only: it changes no document.

- **Camera.** Every view is an explicit camera passed to the render call; the bridge has no method
  that sets the Hammer viewport camera, so nothing moves and there is nothing to restore, on success,
  error or timeout. The result still says `cameraRestored: true, cameraMoved: false` so a weak agent
  does not worry. If the bridge ever gains a camera setter, `look` must restore it before returning.
- **`target`** resolves to a box: a `targetname` or `nodeID` via `get_entity` (origin +- 64, an origin
  has no extent), a point `[x,y,z]` (+- 32), a box `{min,max}`, or `"map"` (`get_scene_info
  {source:"saved"}` bounds: entity origins + saved mesh extents; needs a saved document). Unknown
  entity: `EntityNotFound` with `didYouMean` from the map's targetnames (first 500).
  **A prefab node** (`CMapPrefab`, which the live `entity.get` refuses as "not an entity") is resolved by nodeID (a
  number or a numeric string) from the SAVED active `.vmap`: the box is origin +- 64 like a point entity, `target.kind`
  `"prefab"`, `framedOn` `"prefab origin +-64"`. The prefab's contents bounds are NOT read, so a big prefab needs an
  explicit box or `pose`; there is no targetname lookup (the node carries none in the reader). An unsaved prefab is
  `NotAnEntity` whose message says to `save_map` first.
- **Framing** (`fit_distance`): the box is bounded by a sphere (radius = half the diagonal, at least
  16); the camera sits `r / sin(half_fov) * 1.1` from the center, where half_fov is the narrower of
  the horizontal and vertical half angles (fov 70 horizontal, 16:9). The whole box is in frame from
  every direction (unit test projects the 8 corners). zfar = max(30000, d + 2r).
- **Modes.** `viewport` (default without target): `render_view` with no camera. `angles` (default
  with target): `corner_sw` (pitch 25, yaw 45: camera at -x,-y), `corner_ne` (yaw 225), `top` (pitch
  80). `tour`: `count` (2..8, default 6) views, pitch 20, yaw 45 + i*360/count. `frames`: `count`
  (default 4) images of the `angles` first pose (or an explicit `pose`) `intervalMs` apart (default 500,
  max 5000), only with a running game (`game.local` first; NoMapLoaded + useInstead run_map otherwise,
  nothing rendered). `player_eye`: `game.local` `player.eye` (else origin + 64) and `viewAngles`, or an
  explicit `pose`; render_world's default fov (90).
- **Renderer.** `angles`/`tour`/`player_eye`/`frames` use the ENGINE (`render_world`, the compiled
  map). `source:"editor"` renders the same poses with Hammer's live document (`render_view`).
- **maxSize** (64..1920, default 640) is the longest side; the image is 16:9. Images come back inline
  (one per view, `views[].imageIndex`) plus `file`/`hostPath`/`path`. `name` is a stem; each view
  appends its label.
- **Failures.** A view that fails is listed in `failed` and the others are returned (`notes` says
  how many); no view at all is the first render error, never an empty success. A stop error
  (ToolBudgetExceeded, NotImplemented, BridgeBusy, PathNotMapped, RenderFileMissing) ends the loop and
  every planned-but-unrendered view is listed in `failed` as "not rendered: <cause>".
- **Blank renders.** If EVERY engine view is blank (no compiled map loaded: editing before
  compile_map + run_map; or the camera inside geometry) the call fails with `BlankRender` and
  `useInstead look {..., source:"editor"}` (frames: `run_map`). A partly blank set succeeds with
  `blankViews` and a first note starting `WARNING:`. Editor renders that are blank only get the note.
- `next`: with the engine source it says to look again with `source:"editor"` for document edits (the
  engine shows the COMPILED map) or compile_map + run_map first.
- Nits: `name` + longest label must fit the 64-char stem (name <= 49, checked in parse); an editor
  `player_eye` reports `fov: "the viewport's"`; `poseFrom` says when `game.local` had no view angles
  (angles [0,0,0]); a numeric-string target is also tried as a nodeID; `didYouMean` queries
  `get_scene_info {query}` with words of the name, so the pool is named entities.
- `next`: describe in writing what the images show, then `set_node_transform` /
  `set_entity_keyvalues` to fix what is wrong, then look again.

## Tool notes (moved from descriptions, WP2.2)

Tool and schema descriptions were cut to what an agent needs per call. The text below is what was removed. The full pre-WP2.2 text of every description is in git at 7b5ccf6.

### Group A (mesh, node transform, ModelDoc, particles)


### create_mesh
Old description: Create a mesh (brush geometry) in the LIVE map from WORLD-space vertices and faces (each face a vertex-index loop, counter-clockwise seen from the visible side; coincident vertices are welded by Hammer, and the node's pivot is re-centred on the bounds). orientOutward=true fixes the winding of a closed convex solid automatically. One undo group; undo_last removes the mesh; never saves.

### create_block
Old description: Create a primitive mesh in the LIVE map: shape=box (min,max), wedge = ramp (min,max, riseToward +x/-x/+y/-y, full height at that end), cylinder (center = bottom centre, radius, height, sides 3..64). Optional per-side materials: topMaterial / bottomMaterial / sideMaterial win over `material` per face by its normal's z. One undo group; undo_last removes the mesh; never saves.

### set_mesh_geometry
Old description: Replace ALL polygons of a mesh (nodeID) in the LIVE document with new WORLD-space vertices and faces (counter-clockwise seen from the visible side). The node keeps its id; its pivot is re-centred on the bounds and texture alignment is re-initialised. One undo group; undo_last restores the previous geometry; never saves.

### set_mesh_material
Old description: Assign a material to a LIVE mesh's faces (undoable, never saves). Without `faces` the bridge's mesh.setMaterial assigns it to every face and keeps texture alignment; with `faces` (indices from get_mesh) the mesh is rebuilt through mesh.setGeometry, which needs the geometry of a mesh this server created or rewrote in this session (the bridge has no live geometry read: mesh.get is NotImplemented), and re-initialises texture alignment.

### transform_mesh
Old description: Translate, yaw-rotate (degrees about the pivot) and/or scale a LIVE mesh (nodeID), rebuilding it: the node keeps its id, its pivot is re-centred on the bounds and texture alignment is re-initialised. The geometry transformed is what this server knows for the mesh — the geometry it wrote in this session, else the SAVED .vmap (only while the live document is clean: a dirty or dirty-unknown document is refused with a hint to save_map first) — and `geometrySource` says which. A scale on an odd number of negative axes mirrors the mesh and flips every face's winding. One undo group; undo_last restores the previous geometry; never saves.

### move_mesh_vertices
Old description: Offset selected vertices of a LIVE mesh (nodeID) by [dx, dy, dz] — e.g. drag the top edge of a box to make a slope — and rebuild it (pivot re-centred, texture alignment re-initialised). `vertices` are indices from get_mesh. Faces must stay planar for clean results (Hammer triangulates non-planar polygons); a move that flattens a face is refused before anything is sent. The geometry moved is what this server knows for the mesh — the geometry it wrote in this session, else the SAVED .vmap (only while the live document is clean: a dirty or dirty-unknown document is refused with a hint to save_map first) — and `geometrySource` says which. One undo group; undo_last restores the previous geometry; never saves.

### extrude_mesh_face
Old description: Push one face of a LIVE mesh (nodeID) out along its normal by `distance`, adding one side quad per edge, and rebuild it (pivot re-centred, texture alignment re-initialised). A negative distance pushes inward (a recess). `face` is an index from get_mesh; the cap keeps its face slot and material and sideMaterial defaults to the cap's. The geometry extruded is what this server knows for the mesh — the geometry it wrote in this session, else the SAVED .vmap (only while the live document is clean: a dirty or dirty-unknown document is refused with a hint to save_map first) — and `geometrySource` says which. One undo group; undo_last restores the previous geometry; never saves.

### delete_mesh_faces
Old description: Remove faces of a LIVE mesh (nodeID) and rebuild it: vertices no face uses are dropped and the remaining face indices are renumbered (call get_mesh again before more face edits); the pivot is re-centred and texture alignment re-initialised. `faces` are indices from get_mesh. Deleting every face is refused — use delete_nodes instead. The geometry is what this server knows for the mesh — the geometry it wrote in this session, else the SAVED .vmap (only while the live document is clean: a dirty or dirty-unknown document is refused with a hint to save_map first) — and `geometrySource` says which. One undo group; undo_last restores the previous geometry; never saves.

### align_texture
Old description: Set the texture mapping (UV) of a LIVE mesh's faces (undoable, never saves). mode: world = snap to the world grid (offset 0, default scale), face = align to each face's plane, justify = fit/align to the face (needs `justify`: left, right, top, bottom, center, fit, fitU, fitV), set = only the fields below. shift = texels, scale = world units per texel (> 0), rotate = degrees (sign unverified; pivots at texture (0,0), so the texture also slides). shift/scale/rotate apply after world/face/set, not with justify. keepScale: world/face keep the scale. faces = get_mesh indices (default all). Answers each face's axisU/axisV [x,y,z,offset] and scale.

### list_meshes
Old description: List CMapMesh nodes (brush geometry) from the SAVED .vmap (file route; reports staleRead): nodeID, origin, bounds, vertex/face counts, materials. Filter with within={min,max} (bounds overlap).

### get_mesh
Old description: Full geometry of one CMapMesh by nodeID. An omitted `path` reads the ACTIVE document live (source "live": what Hammer holds now, unsaved edits included — the geometry and face order set_mesh_material {faces} and the rebuild operators index into) when the bridge supports it, else the SAVED .vmap (source "file", with staleRead); `path` always reads that file. WORLD vertices, faces (vertex-index loops, counter-clockwise from the visible side), faceMaterials, per-face centre/normal (faceInfo), bounds. Limits: the live read applies only the node's own transform, so under an instance/prefab/unknown container the vertices may be parent-space (worldSpace:false + a note; the rebuild operators refuse such a mesh); a mesh too large for one bridge reply (about 1 MiB, thousands of vertices) is read from the saved file when the document is clean, else refused with MeshTooLarge and its counts (save_map and retry).

### set_node_transform
Old description: Move/rotate/scale one live node (entity or mesh brush) by nodeID through Hammer's own setters (one undo group "MCP: set transform"; never saves). At least one of origin/angles/scales; relative=true treats origin/angles as deltas from the current values (read first) and scales as factors multiplied with the current ones. Needs bridge m2.18 (node.setTransform); moving a prop_static changes the editor only until the next compile_map.

### list_models
Old description: Model files available in the game VPKs and the current addon, as cataloged (models/**/*.vmdl). Use an entry's `model` value verbatim as the prop's `model` keyvalue.
Defaults dropped from props: source=any, compiledOnly=false, limit 200, offset=0.

### find_assets
New in P5 (`src/tools/assets.rs`). Kinds: material, model, particle, sound (`.vsnd` files; event names are `list_soundevents`), texture. Models come from the `list_models` catalog (`model_catalog.rs`, its addon part rescanned per call); the other kinds from per-kind indexes over the two game paks (built once, cached by pak mtime+len; first call on the real install ~0.3 s in a debug build, later ones 1-20 ms) plus a fresh bounded walk of the addon's compiled and source trees every call. `path` is lowercase, `/`, source extension. `source`: `addon` (compiled in the addon), `content` (addon source only), `csgo`, `core`; `compiled` is false only for source-only addon files. Sorted, keyset-paged (`cursor` = last path of the previous page), `total` counts every match. Materials: only the returned page is checked, through `asset_resolve::Sources::check_textures` (the same resolver as report_ready's `materials` check: RERL texture list, addon dir + game dirs + both paks, case-insensitive disk lookup). `renders`: `ok` (compiled, texture list read, every `.vtex` resolves), `missing_textures` (+ `missingTextures`), `unknown` (+ `reason`: source only, unreadable, or a game pak could not be opened). A material with no texture references is `ok` (nothing can be missing), but a texture list that is missing, truncated or malformed (`Resource::external_refs_checked`) is `unknown` with the reason, and report_ready counts it as not checked (skipped), never as zero textures. Models: the cached `list_models` catalog's pak entries plus a fresh `ModelCatalog::addon_scan` (no rebuild or rescan per call; 2 ms cached on the real install). `next` for a model is get_model_info on the first compiled hit (else model_compile on the first); for a material it is set_mesh_material with the material only in `why` (the tool needs a nodeID). With `source:"addon"` an uncompiled reason says "not compiled in the addon"; with `source:"game"` hits are labelled with and checked against the pak copy. `new_map_from_template` was not built: `new_map {template}` already is that.

### get_model_info
Old description: Compiled-model facts for placing a prop (ReadOnly: model catalog + the model's .vmdl_c bytes). Reports render/physics bounds, `placement` (floorOffset lifts the origin so the model rests on a floor at its z; footprint = the x/y box size), skins, bodygroups, sequences, attachments and the embedded keyvalue text. An uncompiled addon model is refused (ModelNotCompiled); an unknown name gets up to 5 catalog suggestions.

### model_render
Old description: Offscreen render of ModelDoc's main 3D viewport for the active session (bridge modeldoc.render, m2.14): writes a PNG on the host under <app>/renders/, returns the bridge's stats, the mapped host path and image statistics, and inlines the PNG as an image block (downscaled above 1920x1080) unless inline:false. An active session (an open .vmdl tab in the running ModelDoc) is required; the document itself is not. The camera is the viewport's own.
Inline policy/same pipeline as particle_render; chain failure is -32019 NoViewport; bridge modeldoc.render m2.14.

### model_status
Old description: ModelDoc status: whether modeldoc_editor.dll is loaded and verified, missing modeldoc.* capabilities, the active document (+ host path, inAddon) and a launch/open hint. Never raises.

### model_read
Old description: Read a .vmdl from disk (file route, source:"file"): one node subtree as JSON (_class, name, attrs, children). Paths: host, Z:\app\..., or csgo_addons/<addon>/models/x.vmdl.

### model_list_nodes
Old description: Flat node list of a .vmdl from disk (path /i/j, _class, name, childCount, attrKeys), filterable by class/name.

### model_edit
Old description: Edit a .vmdl file in the active addon: ops add_node/remove_node/move_node/set_attr/remove_attr, all-or-nothing, validated (node classes vs the snapshot), backup + atomic write. Does not touch the editor: model_reload afterwards.

### model_create
Old description: Create a new .vmdl in the active addon from a template (static_prop, skeletal). Refuses an existing file unless overwrite.

### model_compile
Since I8 a successful compile adds a note and a `compile_map` next step: a running engine keeps a model it already loaded, so restart the tools session and recompile the map to see a recompiled model.
Old description: Compile a .vmdl to .vmdl_c. Route resourcecompiler (default; runs in the ModelDoc container, works for new files) or editor (bridge Compile; needs hasAsset, refuses dirty unless saveFirst). ok = .vmdl_c updated.

### model_open
Old description: Open a .vmdl of the active addon in the running ModelDoc (pre-validated; refuses while the live document is dirty or dirty unknown unless force).

### model_reload
Old description: Reload the active ModelDoc document from disk (after model_edit). Discards the editor's undo history; refuses while dirty or dirty unknown unless force.

### model_save
Old description: Save the active ModelDoc document to its own path (never Save As: refuses untitled documents and files outside the addon).

### model_node_types
Old description: Every ModelDoc node class name (_class) the running editor knows, sorted; writeSnapshot stores them in hammer-mcp/research/modeldoc-nodetypes.json.

### particle_read
Old description: Read a .vpcf from disk (file route, source:"file"): the kv3 header, the root's non-array fields, per-array entry counts, and with function:"m_Operators/2" that entry's fields. Paths: host, Z:\app\..., or csgo_addons/<addon>/particles/x.vpcf.

### particle_list
Old description: Flat list of a .vpcf's function entries from disk (array, type, index, class, fieldKeys), filterable by array (one of the nine keys) and class. counts reports how many entries each present array holds.

### particle_validate
Old description: Validate a .vpcf on disk against research/pet-schema.json (structure, root class, array registration, every entry's class and fields); returns ok, errors, warnings and schemaSource. The schema is required: with no usable snapshot this is SchemaUnavailable, never ok:true.

### particle_edit
Old description: Edit a .vpcf file in the active addon: ops add_function/remove_function/move_function/set_field/remove_field/set_root_field/remove_root_field/add_child/remove_child/add_variable/remove_variable, all-or-nothing, schema-checked (skipSchema skips only that check), backup + atomic write. Does not touch pet: particle_reload afterwards.

### particle_create
Old description: Create a new .vpcf in the active addon from the minimal template (maxParticles, renderer). Refuses an existing file unless overwrite.

### particle_compile
Old description: Compile a .vpcf (or a folder of them; recursive) to .vpcf_c with resourcecompiler.exe. children (default true) also compiles referenced m_ChildRef files, children first; stock/external children are reported, not compiled. Returns files[] verdicts; ok only if every file is.

### particle_status
Old description: Particle editor (pet) status: whether pet.dll is loaded, missing pet.* capabilities, the active .vpcf document (+ host path, inAddon, the nine arrays by key) and a launch/load hint. Never raises.

### particle_types
Old description: Every particle function class the running pet registers (pet.functionTypes), grouped by array. writeSnapshot stores the full pet-schema.json snapshot (pet.schema per class) under hammer-mcp/research/.

### particle_schema
Old description: Schema (bases, flattened fields, metadata) of one particle class: from the running pet (pet.schema) when it is advertised, else from the local pet-schema.json snapshot.

### particle_load
Old description: Load the particle editor tool (pet.load) into the running tools session and wait for pet.dll to verify. One shot: a loaded:false reply means a modal may be up (restart the session).

### particle_open
Old description: Open a .vpcf of the active addon in the running pet (pre-validated on the host; refuses while the live document is dirty or dirty unknown unless force).

### particle_reload
Old description: Reload the active pet document from disk after a file edit (pre-validated on the host: a malformed file would crash pet in ReloadDocument — main-thread stack overflow, the whole game process wedges — so nothing is sent until the file validates). Refuses while dirty or dirty unknown unless force.

### particle_save
Old description: Save the active pet document to its own path (never Save As: refuses untitled documents and files outside the addon); force re-saves a clean document in pet's own form, e.g. to normalize a hand-written file.

### particle_live_read
Old description: Read the live pet document as pet serialized it (pet.text): source:"live", the document path (+ host path), dirty/isNewFile/canUndo/canRedo, bytes, the kv3 header, the root's non-array fields, per-array entry counts and, with function:"m_Operators/2", that one entry.

### particle_live_edit
Old description: Edit the live pet document in-editor with full undo (pet.text + pet.apply): ops add_function/remove_function/move_function/set_field/remove_field/set_root_field/remove_root_field/add_child/remove_child/add_variable/remove_variable, all-or-nothing, schema-checked (skipSchema skips only that check; structure is never skippable). The document is dirty afterwards: particle_reload/particle_open then need force, or particle_save first.

### particle_undo
Old description: Undo the last live pet edit (pet.undo; the same history the editor's Ctrl+Z uses). A bridge done:false (nothing to undo) is a retry-safe error.

### particle_redo
Old description: Redo the last undone live pet edit (pet.redo). A bridge done:false (nothing to redo) is a retry-safe error.

### particle_render
Old description: Offscreen render of the running particle editor's preview viewport (bridge pet.render, m2.13): writes a PNG on the host under <app>/renders/, returns the bridge's stats, the mapped host path and image statistics, and inlines the PNG as an image block (downscaled above 1920x1080) unless inline:false. The camera and viewport are pet's own preview; a document is not required.

### Dropped property-description knowledge (group A)
- create_mesh: vertices 3..65536, |coord| < 1e6; faces need >=3 distinct indices, no repeats, no zero-area face; material <= 259 bytes; faceMaterials <= 64 distinct; coincident vertices welded by Hammer; pivot re-centred on bounds; orientOutward reports how many faces were flipped.
- create_block: sides 3..64; top/bottom/sideMaterial pick faces by normal z (>0.5, <-0.5, else vertical); wedge riseToward +x/-x/+y/-y, full height (max.z) at that end.
- set_mesh_geometry / rebuild ops (transform/move/extrude/delete): node keeps its id; pivot re-centred and texture alignment re-initialised; undo_last restores previous geometry; the geometry used is what this server wrote this session, else the SAVED .vmap, only while the live document is clean (dirty / dirty-unknown refused with a hint to save_map); geometrySource says which. Scale with an odd number of negative axes mirrors and flips winding; move that flattens a face is refused before sending; Hammer triangulates non-planar polygons; delete of every face is refused (use delete_nodes); extrude cap keeps its face slot/material, sideMaterial defaults to the cap's, distance 0 refused.
- set_mesh_material: without faces uses mesh.setMaterial (keeps texture alignment); with faces rebuilds via mesh.setGeometry (re-initialises alignment). mesh.get was NotImplemented at one point; faces indices are get_mesh's live order, a source:"file" answer lists the SAVED file's own order which a live mesh need not share.
- align_texture: mode set = only the shift/scale/rotate fields; world resets offset 0 + default scale (keepScale keeps scale); justify values left,right,top,bottom,center,fit,fitU,fitV; rotate sign unverified and pivots at texture (0,0) so the texture also slides; texSet 0/1 = UV set; faces default all.
- get_mesh: live read applies only the node's own transform, so under instance/prefab/unknown container vertices may be parent-space (worldSpace:false + note; rebuild operators refuse such a mesh); too large for one bridge reply (~1 MiB, thousands of vertices) is read from the saved file when clean, else MeshTooLarge with counts. path forms: addon-relative maps/x.vmap, content-relative csgo_addons/<addon>/maps/x.vmap, host absolute, mapped Windows path; omitted = active document (needs bridge). nodeID 0..=2147483647. list_meshes limit default 500, max LIMIT_MAX.
- set_node_transform: needs bridge m2.18 (node.setTransform); one undo group "MCP: set transform"; origin each within +-1048576; scales each in (0,1000]; relative reads current values first, a field the read cannot supply is refused (mesh nodes have no live transform read, so relative scales are refused for them); prop_static is baked by resourcecompiler, so the move holds only until next compile_map. nodeID from find_entities / node.list / get_entity; 32-bit ids.
- list_models: source default any (any/addon/game), compiledOnly default false, offset default 0, limit default 100; query globs e.g. 'models/props/*chair*'; prefix lowercase /-separated under models/.
- get_model_info: ReadOnly, no bridge methods (catalog + .vmdl_c bytes); include default sequences/attachments/bodygroups, keyvalues only on request; limit per list (default 200, cut reported in truncated).
- model_render / particle_render: bridge modeldoc.render (m2.14) / pet.render (m2.13); width/height 16..4096, width*height <= 8388608; name stem 1..64 chars, no leading dot, optional .png dropped; inline default true; shared pipeline run_render; model_render chain failure is -32019 NoViewport (needs an active session = open .vmdl tab; document itself optional).
- particle_compile: path addon-relative `particles/x.vpcf` or `csgo_addons/<addon>/particles/x.vpcf`; force passes -f (use when a result says the compiler skipped a stale output); timeoutMs default 120000, clamp 600000; recursive default false.
- particle_reload: pre-validated because a malformed file crashes pet in ReloadDocument (main-thread stack overflow wedges the game process). particle_live_edit: structure is never skippable; label 1..64 printable ASCII no quote/backslash; allowObsolete default true; bridge methods pet.text + pet.apply; undo/redo are pet.undo/pet.redo (done:false = retry-safe error).
- particle_validate uses research/pet-schema.json (schemaSource reported). particle_types: writeSnapshot writes pet-schema.json (pet.schema per class). model_node_types: writeSnapshot -> hammer-mcp/research/modeldoc-nodetypes.json. model_compile editor route = bridge Compile.

### Group B (triggers, sounds)

### create_trigger
- Volume: exactly one of `box {min,max}` (|coord| <= 1048576, each side >= 1 unit, max > min), `geometry {vertices,faces}` (mesh.create rules: 3..65536 vertices, |coord| < 1e6, faces CCW seen from the visible side, >= 3 distinct indices, no degenerate face), or `meshNodeIDs` (1..64 distinct existing live world meshes). Built via bridge entity.tieMeshes (m2.19).
- Class default trigger_multiple; unknown class = UnknownClass, non-solid = NotASolidClass (nothing sent).
- spawnflags = COMPLETE set (labels case-insensitive or bit numbers); a default-on bit not listed is OFF and a warning names it. Omitted = FGD defaults sent explicitly and noted (trigger_once: "4097"). Unknown label = UnknownSpawnflag listing valid ones.
- material default: class FGD auto_apply_material (core/base.fgd), else materials/tools/toolstrigger.vmat; applied to created meshes; existing `meshNodeIDs` only with retexture:true (one mesh.setMaterial undo group per mesh).
- keyValues validated against the FGD (issues are warnings, like create_entities); values string/number/bool; classname, spawnflags, targetname and engine-managed origin/angles/scales are refused. Echoed back as `applied` plus a warning per key the bridge did not write.
- connections (1..64): output checked against class FGD outputs BEFORE anything is sent; target may be !activator/!self or a name not yet in the document (noted); delay 0..3600; timesToFire -1 = every time; `force` skips FGD output/input checks (the prop_static target refusal is never skipped). One extra undo group.
- Runs as 2..n undo groups (volume, tie, each retextured mesh, connections); reports undoGroups/undoLabels. Failure after the volume exists = PartialFailure naming that mesh (not retry-safe).
- allowRetie (meshNodeIDs form only): moves a mesh owned by another brush entity; the emptied old entity is deleted in the same undo group and listed in removedEmptyEntities; otherwise MeshAlreadyTied.

### tie_to_entity
- Exactly one of `class` (new entity of an FGD @SolidClass; unknown/non-solid refused with nothing sent) and `entityID` (existing LIVE brush entity; point entity or non-entity = NotABrushEntity; class/targetname/spawnflags/keyValues refused with it).
- One undo group creates/uses the entity and re-parents the meshes. A mesh already under the target is skipped (`alreadyTied`); an all-alreadyTied call is a success no-op with no undo group. Mesh under another brush entity = MeshAlreadyTied {nodeID,parentNodeID} unless allowRetie (deletes the emptied entity, listed in removedEmptyEntities).
- `material` retextures every tied mesh via mesh.setMaterial (one undo group each); without it meshes keep their own (Hammer itself never applies one on tie). keyValues echoed as `applied` + warning per unwritten key. spawnflags rules as create_trigger. Bridge entity.tieMeshes (m2.19); it is the primitive create_trigger's tie step uses.

### move_to_world
- Moves the tied meshes under the entity's PARENT (world, or the group/prefab it sits in) and deletes the entity; one "Untie from entity" undo group (bridge entity.untie, m2.19). A point entity (no mesh children) = NotABrushEntity. `deleteEntity:false` is refused (InvalidArguments) until Hammer can keep an empty entity.

### list_triggers
- Sources: "file" = SAVED .vmap (no bridge; reports staleRead), "live" = LIVE document (needs bridge m2.19 entity.get.children plus live mesh.get). Default file when `path` given, else live. `path` forms: addon-relative (maps/x.vmap), content-relative (csgo_addons/<addon>/maps/x.vmap), host absolute, mapped Windows path; file route only.
- kind "trigger" (default) = @SolidClass classes resolving to a Trigger/TriggerOnce base or a tags(Trigger) helper; "solid" = every @SolidClass (func_buyzone, func_brush, ...) plus unknown-class entities with mesh children.
- class/targetname: exact, case-insensitive, empty string refused. `within` box: matched against the volume bounds when known, else the node ORIGIN only (origin = first mesh's pivot, can lie outside a box the volume overlaps). withinMode contains (volume ENTIRELY inside) | intersects (overlap); without a volume both test the origin. includeConnections default true (false omits the field), includeKeyValues default false; limit default 200 (max 2000); offset default 0. Volumes = union of mesh children's bounds.

### set_trigger_volume
- Replaces the volume via mesh.setGeometry on one tied mesh in WORLD coordinates; mesh keeps id/parent/node, pivot re-centred on the new bounds, texture alignment re-initialised. Children come from entity.get (m2.19): none = NotABrushEntity, several = name one with `meshNodeID` (refused if not a child, unless the children list was truncated, then sent with a note). With meshNodeID the children read is not needed (an m2.18 bridge works); without it a bridge reporting no children = BridgeTooOld.
- Material default: the mesh's own (live mesh.get read this session, else the SAVED .vmap, file only while the document is clean; a dirty document = UnsavedChanges for a NON-trigger class). A trigger class falls back to FGD auto_apply_material, else materials/tools/toolstrigger.vmat (materialSource "fgd-default (mesh material unknown)"). Every face takes it.
- One undo group; undo_last restores the previous geometry. box/geometry limits as create_trigger.

### create_sound_entities
- Kinds map to fixed classes; other FGD tags(Sound) classes via `class` (NotASoundClass if no Sound tag). Family from the FGD base chain: env_soundscape descendants (snd_soundscape, *_triggerable) get enablesoundevent+soundevent; point_soundevent descendants get soundName; other tags(Sound) classes take soundEvent only if the FGD declares a sound-typed key (else a soundEvent is refused rather than silently written nowhere).
- soundEvent required for point_soundevent and soundscape families (unless legacySoundscape); checked against the soundevent index BEFORE sending; unknown = refusal unless allowUnknownSoundEvent (warning per entity). legacySoundscape (soundscape family): written to `soundscape` with enablesoundevent "0", unvalidated.
- Key spellings written for you: soundName/soundevent, enablesoundevent, radius, box_mins/box_maxs, position0..7; these plus startOnSpawn, classname, origin, angles, scales are refused inside keyValues. startOnSpawn writes "1"/"0" (default: FGD default, not sent).
- radius: only when the class declares a radius key; snd_event_sphere a number > 0; soundscape family and env_soundscape_proxy an integer >= 1 or -1. box: only for classes declaring box_mins/box_maxs (alignedbox/orientedbox, snd_event_box_helper); min < max per axis, |coord| <= 1048576; the two box kinds send the FGD default -64..64 explicitly when omitted. positions: soundscape family, 1..8 targetnames to position0..7.
- schedule (needs targetname; refused for classes without a StartSound input, i.e. soundscapes): startDelay/stopDelay build ONE logic_auto `<targetname>_auto` (OnMapSpawn -> StartSound/StopSound with delays); repeatEvery (+repeatInitialDelay) builds ONE logic_timer `<targetname>_timer` (OnTimer -> StartSound); first fire at InitialDelay + RefireTime after load, then every RefireTime (e.g. repeatEvery 5, repeatInitialDelay 1 -> first at 6 s). targetname also required for connections.
- connections: checked against the class's FGD outputs before sending (snd_event_* outputs: SoundEventGuid, OnSoundFinished). strict:true refuses on FGD keyvalue ERRORS (warnings pass either way). entities 1..32 (helpers on top).
- One edit.batch for entities AND helpers, then one entity.addConnections group per entity with connections/schedule (`undoGroups` says how many; undo_last pops them). undoLabel is echoed on the batch (bridge labels it "MCP: batch"). Needs bridge m2.18.

### drain_sounds
- Bridge sound.drain: reads AND consumes the sound-event ring (events since last drain): name/hash, entindex/entityName, the SOS slot that started them (`via`), ageMs and, since bridge m2.20, packed server sound params decoded into `params` and a world `position` (a snd_event_* carries the entity's Hammer origin there; origin/entityName alone are the world entity and [0,0,0]).
- Ring holds 256; `max` 1..256, default 256 (bridge's own default is 128, so the value is always sent explicitly). Call promptly after the moment observed; repeat while truncated is true.
- filter (namePrefix, names 1..32 exact case-sensitive since names are hashed, within sphere {center, radius>0} in Hammer world units) is applied AFTER the drain: filtered-out events are consumed and counted in unmatched. `within` matches the decoded position first, else a non-null non-zero origin; an event with neither is always unmatched (noted).
- resolve default true (entity origin/entityName); false skips the read and leaves origin absent, so within can only match the decoded position.
- installOnly:true sends sound.status instead and installs the wrap WITHOUT draining: arm before run_map so load-time startOnSpawn events remain in the ring. filter refused with it; max/resolve ignored with a note.
- Bridge without sound.drain = NotImplemented, nothing sent; -32017 PrerequisiteFailed (soundsystem pins) and other bridge errors pass through.

### edit_soundevents
- No inputSchema (additionalProperties open): args live in the description. Edits soundevents_addon.vsndevts (or another .vsndevts under the addon) without destroying comments. Ops: set (properties object; merge:true shallow-merges into the existing event, overwrite:true (top-level flag) replaces it, else an existing event is refused), remove (force:true removes a referenced event), rename (rewrites base/soundevent_01 references). Names [A-Za-z0-9_.]{1..127}; base resolution and type/base presence validated before anything is written (validate:false skips the index-backed checks with a warning). A comment inside a replaced event is reported in commentsDropped; everything else survives byte-for-byte. dryRun checks without writing; createFile writes the template header first.

### compile_soundevents
- resourcecompiler.exe in a running tools container writes <file>_c. ok:false is a result; `errors` lists every RESOURCE COMPILE ERROR with line/col; `events` counts the compiled file's events. Args path?, timeoutMs?.

### list_soundevents
- Sources: game VPKs and the addon's soundevents directories (`*.vsndevts_c`/`*.vsndevts`), pure filesystem. query: case-insensitive substring, '*'/'?' make it a glob over the whole name (e.g. 'Weapon_AK*'). source 'addon' = addon compiled or source files, 'game' = csgo/core VPKs. file: substring of the holding file of the top-ranked copy. `name` (exact, case-sensitive) = detail route: ONE event with `properties`, `resolved` base chain, `children`/`vsnds` verdicts; ignores list filters; unknown names fail with suggestions. includeProperties default false (name route always includes). limit default 100 (max 1000), offset 0.


### Group C notes (moved from descriptions, WP2.2)

### play_test
- Input step lock wait: lockWaitMs default 20000, or mp_freezetime + 5 s when longer (read via cvar.get), cap 60000, 0 = do not wait; `movementLock` in the step result reports the effective lockWaitMs and its source. Waits once per run while game.rules says freeze period, team intro or restart window, because keys pressed then stay dead even after it ends (run2: input after mp_restartgame did nothing for 8 s). Fix when it fails: raise lockWaitMs or shorten mp_freezetime.
- timeoutMs up to 120000 is accepted but cut to the call budget (45 s default); the run then ends endedBy budget, not failed.
- Full example: {steps:[{kind:'input',steps:[{press:['forward'],angles:[0,90,0],holdTicks:1}]},{kind:'wait_state',path:'player.origin[1]',gt:1200,timeoutMs:20000},{kind:'wait_log',pattern:'package planted',timeoutMs:15000},{kind:'expect_state',path:'player.alive',eq:true}]}.

### game_state
- Bridge method game.local; also `game_local` is the old name (alias kept). hint on team 0/dead names the exact fix (jointeam, or mp_restartgame) plus a next step.
### input_set
- Bridge input.set at usercmd level: the game sees real input, so IsInputPressed in a cs_script reads it. While an angles override is set, console setang is ignored (override wins every tick). release + tap of the same held button in one call re-presses only on bridge m3.15+; older bridges keep it held (use two calls). Clear everything with input_clear.
- tap holds for `frames` input frames.
### input_sequence
- Same tool as client-mcp's. Hold ticks are counted from the first game.local reading after the step's input.set. input.clear sent at the end and after any error (inputCleared).
### entity_find
- Bridge game.entities. toggleState is null until the bridge can read it; max 1..256 (default 64). Same name and fields as client-mcp's.
### game_rules
- Bridge game.rules, schema-resolved via the cs_gamerules proxy; unresolved fields are null and listed in schemaMissing. Fields also include game phase, rounds played, round start/restart times, round time, end reason. play_test waits for the freeze period by itself before pressing keys. Same as client-mcp's.

### run_map
- Mechanism: console `map <name> nomapvalidation=1` over the VToolMessage command channel; gameType/gameMode are separate `game_type N`/`game_mode M` VToolMessages posted before the map command (defuse playtest used 0/1 = competitive). Remount = `disconnect` + the engine's idle-level signal. unload posts only `disconnect`; it is a no-op with a note when game.local says the engine is not in a map; it is what compile_map's map-in-game refusal points at.
- The Hammer document is not touched by a load; unsaved edits are reported but the engine loads the .vpk. A post whose delivery is unknown is never repeated. While the previous load has no verdict, the call waits for it then refuses EngineBusy. ignoreRunningCompile false: refused with the job's id and a job_status next step.
- timeoutMs: default 60000 or what the tool budget leaves, whichever smaller; max 600000; always clamped to the remaining tool budget. unload states: ok|failed|timeout. `team.skipped` when load unconfirmed; `team` in the result.

### compile_map
- Route: runs as a bridge job when the bridge advertises job.start (route "bridge"), else in the tools container (route "podman"); agents need not care. Result also has vpk form + candidates. Also refuses EngineBusy when a tagged compile tree from another server is still alive in the container.
- Fresh compiles took 2-7 min; the vrad3 lighting pass is the slow part (hence default timeoutMs 1500000). Path canonical: forward slashes, no '.'/'..'/'//' segment, 1..512 chars. cancel on a finished job answers its final state with cancelled:false.
- waitMs: cut to what the tool budget leaves; reply has done and waitedMs.
### compile_asset
- Result also has log and command. The compiled file is written next to the game tree (check with the asset's own tool). Needs the bridge's job.start (no other route). Bridge flag allowlist is the one in the flags schema.
### job_status
- jobId is also a compile_map bridgeJobId. log.host = the log file on the host. Both lists returned with no jobId.
### job_cancel
- cancelled:true when this call ended it.

### render_view
- Map-thumbnail path; mapped host path and image statistics are in the result. Default file name render_<tick>_<seq> (bridge-named so concurrent servers cannot collide); a trailing .png in `name` is dropped. Does not change the document.
### render_world
- Goes through the game's own rendering pipeline into an offscreen target. The world is live: animated content can differ slightly between identical renders. -32021 NoGameWorld is the rare case of an engine with no scene world object at all (an empty engine otherwise still renders a black scene with the blank note). Same name/file-stem rules as render_view.

### new_map
- Path canonical: forward slashes, no '.'/'..'/'//' segment, 1..512 chars. Writes only inside the active addon's maps/ and only atomically. The template's `.vpost`/soundscape resources are copied into the addon when absent (never overwritten). open:false only creates the file; Hammer loads only with a document open.
- clear:"gameplay" also removes info_target and other func_/trigger_/prop_/logic_/point_ entities; keeps match-flow cameras and minimap boundary, map parameters. Node kinds clear cannot delete are listed under `kept`. One undo group per 256 nodes. Result clear {planned, removed, kept, skipped?}, templateAssets {copied, missing}.

### apply_layout
- Path outside the project root is refused; accepts host, Windows-mapped, or addon/project-root-relative paths. Execution: one mesh.create per mesh (each its own undo group, label "MCP: create mesh") and ONE edit.batch for all entities ("MCP: batch"); `name` is only echoed as requestedUndoLabel. strict: an FGD keyvalue error refuses the layout (as create_entities); allowUnknownSoundEvent as create_entities. Bigger layouts are refused up front: split them.

### console_exec
- Sent through the bridge console.exec when available, else the VToolMessage channel (`route`). waitFor is checked over every output line, also ones printed after the command returned. Output = whole lines of game/csgo/console.log written after the call started. Session-ending/crash lines refused (quit, exit, disconnect, maps, ...); map changes too with useInstead run_map. waitMs max 10000, maxLines the LAST ones, rest cut with truncated=true. `echo`/cvar prints reach neither route's console.log in a -tools session (only status-class output and errors do).
### cvar_get
- Bridge cvar.get. Secret names redacted: rcon_password, sv_password, *password*, *token*. names 1..64.
### set_addon
- Returns {addon, source: document|set_addon|flag, previous}. No bridge call, no file. model_edit/model_create write the file and add a warning when the running ModelDoc is on another addon.

### Group D (entities, lifecycle, selection, props)

### create_entities
- One edit.batch (one undo group), expectDoc sent, raw handles dropped. Needs bridge `edit.batch` + `entity.create`.
- Keyvalues validated against the FGD database, sent as strings; unknown class -> UnknownClass; `classname` key or strict FGD issue refuses with nothing sent. FGD warnings (e.g. integral float on an integer key) pass; `strict` turns FGD errors into InvalidArguments.
- `sound`-typed keyvalue (e.g. snd_event_point.soundName) is looked up in the soundevent index: unknown -> SoundEventNotFound unless allowUnknownSoundEvent (then a warning; a misspelt name is silent at runtime). Only sound-typed keys are affected.
- keyValues: max 32 keys, values string/number (sent as JSON text)/bool ("1"/"0"); arrays/objects/null refused; classname/origin/angles/scales refused (use entity fields); value max 511 bytes.
- undoLabel is echoed as requestedUndoLabel; the bridge labels the group "MCP: batch" (a create); a set-only batch is "MCP: set keyvalues" (protocol-diff-m2.15.md section 7, item 16).

### undo_last
- Undo groups close implicitly, so the top group may contain the user's own edits; refuses a non-MCP group unless acceptUserEdits.
- A popped "MCP: set keyvalues" group does NOT revert values on older bridges; on m2.18+ (node.setTransform marker = RecordNodeTree) the values ARE restored (model keyvalue and drawn model included). The result notes say which.

### set_entity_keyvalues
- Live; returns previous values; `strict` exempts a value equal to the current one (reverts never refused). Same keyValues limits as create_entities.
- `force` skips two checks: (WP-R1) a `model` keyvalue on a prop-tagged class is normalized and looked up in the model catalog (unknown -> ModelNotFound with suggestions); (WP-S3) a sound-typed keyvalue is looked up in the soundevent index (unknown -> SoundEventNotFound with suggestions). With force the value is sent unchecked with a warning.
- nodeID range 0..=2147483647 (32-bit bridge ids).

### get_entity / find_entities
- source live = Hammer document (unsaved edits); saved = .vmap on disk (reports staleRead); `sourceUsed` says which ran. find_entities nameContains/withKeyvalues are saved-only (live: get_scene_info query). Default limit 500.

### list_entities
- File route only, reports staleRead; limit default 500; `CMapPrefab` nodes come as `prefabCount` + `prefabs` (I4).

### list_entity_classes / get_entity_class
- list: `kind` default point+solid; `tag` = FGD tags() helper argument matched case-insensitively through the base chain (Sound, PropStatic, Logic ...); a tag nothing carries lists nothing, never an error. playNote (live-verified round-start behaviour: what fires at map load vs when play begins) on logic_auto, logic_eventlistener, func_movelinear. Default limit 200.
- get: includeBases default true (resolved, inherited view); false = declared-by-class only.

### add_entity_connections
- One undo group, never saves. Needs bridge m2.18 entity.addConnections.
- FGD checks: source class must declare the output, a target the live document names must accept the input (force skips both), connection to a compile-time prop_static always refused (no runtime inputs). A target naming nothing (or starting with `!`) is allowed with a note.
- Bridge reads each entry back: unconfirmed ones go under `unconfirmed` with outcomeUnknown:true (maybe executed; read back, never retry).
- Fields: output/input/target max 255 bytes, no control chars; target also `!activator`/`!self`/`!player`; param may be empty; delay 0..=3600 s; timesToFire -1 default = every time, 0 refused.

### remove_entity_connections
- Needs bridge m2.18 entity.removeConnections. `indices` resolved against the live vector inside the write (shift with every edit; max 512, the bridge cap; out-of-range/duplicates refused with nothing opened). `match`: at least one of output/target/input; none matching -> ConnectionNotFound. Result: removed/removedCount and `remaining` (bridge's post-removal count; its own `count` = number removed).

### save_map
- Hammer's own FileSave; the ONLY writer. Refuses editable:false / read-only host file without sending; resets unsaved counter only when bridge reports saved:true.

### reload_map
- Hammer ReloadFromDisk; in-editor undo history dropped. Refuses untitled (new) document outright, and dirty or dirty-unknown unless force; resets unsaved counter only when bridge confirms reloaded:true.

### open_map
- Editor OpenFile on the tools main thread. Local refusals: path not a canonical maps/... .vmap of the addon (forward slashes, no ./../// segments, 1..512 chars), or not parseable as binary-v9 .vmap (InvalidMapFile: OpenFile would pop an "Error opening file" modal and wedge the bridge). Bridge refuses a missing file or one already open in another session (Hammer's session match is a stub; reopening would create a second session and leak the first). existing:true is a result, not an error.
- Fresh tools session: hammer.dll not loaded, bridge does not advertise map.open: first map opens via VToolMessage open_asset post (openedVia:"vtm") and the call waits for the document. Not gated by the registry for that reason.
- Map open in a background Hammer tab: reply says active:false and what can be done (no tool switches tabs).

### delete_nodes
- One undo group, undo_last restores. nodeIDs 1..256 unique, world cannot be deleted.

### get_scene_info
- Live Hammer document or saved .vmap via `source`; `sourceUsed` says which. limit default 25.

### get_selection
- mode: groups, objects, meshes, vertices, edges, faces, mixed, tile, or null; faces mode `faces: [{nodeID, faces:[i]}]`, indices = get_mesh face order (= saved .vmap order). vertices/edges/mixed/tile report count only. `unmapped` = selected items the bridge could not map (0 normally). Mode Hammer reports unknown reads as mode null, count 0. Selection is not an edit. Next: get_mesh/get_entity, set_mesh_material {faces}, transform_mesh/delete_nodes.

### set_selection
- nodeIDs: Hammer switches to objects mode unless already groups/objects/meshes. faces: switches to faces mode. After a mode switch add/remove start from empty (`modeChanged:true`). Not an undo step, nothing dirtied/saved. `applied` may be smaller than `requested` (locked/hidden filtered; `notApplied`), not an error. Unknown nodeIDs, non-mesh face targets, face index >= face count refused with nothing changed. Max 4096 nodeIDs, 65536 face indices. Unknown selection mode: every op incl. clear refused (-32603 selection mode unknown). If the selection changed but readback failed: success with applied:null and appliedError.

### create_props
- One edit.batch, one undo group, expectDoc sent, never saves. kind static|dynamic|physics -> prop_static/prop_dynamic/prop_physics, or explicit class carrying a prop tag (tags(Prop*) itself or via base()), else NotAPropClass, nothing sent; exactly one of kind/class.
- model: any spelling the catalog normalizes (models/**.vmdl); unknown -> ModelNotFound with suggestions; addon model without .vmdl_c -> ModelNotCompiled unless allowUncompiled (placed with per-prop note: does not draw until addon compiles and document reloads).
- scales: each in (0, 1000]; 0/negative/non-finite refused before anything is created; needs bridge m2.18 (node.setTransform), else whole call BridgeTooOld before anything is sent.
- keyValues validated like create_entities; model/classname/origin/angles/scales/skin/targetname refused (use the fields). targetname is a runtime concern (prop_static ignores it). skin = get_model_info skin name. snapToFloor reports floorOffsetApplied.

### list_props
- Saved .vmap (source "file", staleRead) or live document (source "live", bridge m2.15+; m2.18 entity.get reports node origin/angles/scales). Default source: file if `path` given, else live. `path` forms: addon-relative, content-relative (csgo_addons/<addon>/maps/x.vmap), host absolute, mapped Windows path; file source only.
- kind other/unknown = model-entity kinds needing includeOtherModelEntities (non-prop classes with a studio `model` keyvalue = "other"; non-FGD classes with non-empty model = "unknown", problems:"unknown class"). Empty class/targetname/model strings refused. within = AABB on node origin, max >= min. Default limit 200; offset default 0.

### play_test / compile_map / run_map / new_map (second trim, to fit the 700-char cap)

- play_test: result is `{passed, endedBy: complete|failed|budget, steps:[{step, kind, ok, ms, ...}], failedStep?, inputCleared}`. wait_entity with no match keeps polling, expect_entity fails; `index` picks the index-th match (default 0). wait_log `since:start` is for lines that may appear during an earlier step. An input step that presses a key waits once per run (up to lockWaitMs) while the round is in freeze/intro/restart; still locked at the limit fails with movementLocked:true and sends nothing; a step holding a move key that moves nothing explains in `note`. Input is cleared at the end (keepInput keeps it after a pass). All steps are validated before anything is sent. NoMapLoaded carries useInstead run_map.
- compile_map: never the map.compile dialog (resourcecompiler). Result fields while polling: state running|ok|failed|timeout|cancelled, vpk verdict, sourceChanged, logTail, failLines, route, log. Stages in any order; the result reports the resolved `stages`. Fast iteration: novis, or ["nav"] / ["phys","nav"] on an already compiled map (VIS and lighting dominate). `job_cancel` also ends a compile.
- run_map: no .vpk is refused. A load result carries the console lines seen and `remount`. `team` joins with jointeam, plus one mp_restartgame 1 when the player is left waiting for the next round (unless restartIfDead:false); skipped if the load was unconfirmed; game_state's hint names the fix otherwise.
- new_map: the default template is Valve's addon_template (team spawns, bomb sites, buy zones, sky, light, a few meshes; result `contents` lists it). Existing-target check includes case-only differences. clear:"gameplay" also deletes the skybox mesh and keeps cubemap, post-processing, soundscapes; clear:"all" is for starting empty then apply_layout. Result: open_map's view plus created, template, contents, clear, templateAssets.

## Server instructions (WP2.4)

`INSTRUCTIONS` in `src/lib.rs` is what an agent reads at `initialize`, next to `tools/list`. It is
written for an agent with no parent: purpose and non-purpose, the first call (`session_status` and
its `next`), intent gating (a question changes nothing), the traps that cost live runs, "never step
outside the tools" and a definition of done. Every trap in it was seen live; when a trap stops being
true (or a new one costs a run), edit the text, not a tool description. Guard:
`tests/instructions.rs` (3000-character cap; every snake_case word must be a registered tool or
alias, or sit in its short `NOT_TOOLS` list). The definition of done ends on `report_ready` passing
(or every non-pass explained). Details the old text carried (capability gating, the compile job
model) are in the tool descriptions and the sections above.

## report_ready (`src/tools/report_ready.rs`, WP3.2)

`report_ready {map?, profile?: "map"|"release", checks?, debugPatterns?}` is the ship check, ReadOnly: it
reads the SAVED `.vmap` (`src/vmap`), the compiled vpk and the addon tree, and optionally the live
document flag and two cvars; it never saves, compiles or loads. Result `{verdict, profile, map, checks,
counts, file, note?}`; each check is `{name, status: pass|warn|fail|skipped, detail, evidence?, fix?}`.
`verdict`: `fail` if any check failed, else `incomplete` if any check was skipped (or none ran), else
`warn`, else `pass`. **A skipped check is never a pass**, and zero checks is `incomplete`. `fix` is
`{tool, args, why}` where a tool exists (the first three also come back as `next`), else `{do}`, a
manual step. `map` defaults to the active document, else the addon's only map (`NoDocument` with the
list otherwise). `checks` limits what runs; any known name works under either profile, an unknown one
is `InvalidArguments` with `didYouMean` and `valid`.

| check | profile | decides |
|---|---|---|
| `saved` | map | Hammer's `dirty` flag or this server's unsaved-edit counter -> fail (fix `save_map`); no bridge/active document and 0 counted edits -> skipped; not the active document -> pass (the file is what was checked) |
| `compiled` | map | `run_map::vpk_status`, the very lookup/header check/older-than-vmap rule `run_map` uses: missing, bad header or older than the `.vmap` -> fail (fix `compile_map`) |
| `materials` | map | every material the saved meshes name needs a `<path>_c` in the addon game dir, the map vpk, `game/csgo` or `game/core` (loose, case-insensitive, or in `pak01_dir.vpk`), **and each resolved `.vmat_c`'s texture list (`Resource::external_refs`, the `RERL` block) must resolve to `.vtex_c`**: a missing material or texture -> fail naming material + texture (default fix: supply/compile it, `compile_asset` when a `.vmat` source exists; the `set_mesh_material` replacement is `fix.alternative` and carries only the bad faces); a material that cannot be read or has no `RERL`, or no readable game pak -> skipped ("resolves, textures not checked", never a pass for renders); `.vmat` sources with no `_c` and no use are a note in `detail` |
| `prop_materials` | map | every entity with a `model` keyvalue (any class; non-`.vmdl` values ignored): the compiled `<model>_c` must resolve (addon game dir, map vpk, game dirs, game paks). **Addon / map-vpk models** are opened: each `.vmat` in the model's `RERL` must resolve to a `.vmat_c` and each `.vtex` in that material's `RERL` to a `.vtex_c` (via `Sources::check_textures`); first broken link per model: `model not compiled` (fix `model_compile` when the `.vmdl` source is in the addon, else `fix.do`: the model path is wrong), `material X missing` (fix `compile_asset` when a source exists), `texture Y missing` (`fix.do`: `model_import {source, overwrite:true}` / `materialMap`, then `model_compile`) -> fail naming nodeID/targetname + model. **Shipped models** (found only in the game paks) pass at the model level: their materials ship in the same paks, so they are not opened (a typo in a shipped path still fails). No `model` keyvalue anywhere -> pass. A model/material that cannot be read, a truncated or malformed RERL list, or unreadable game paks when a model is not in the addon -> skipped, never pass. Distinct models are cached; at most 2000 are opened (the rest are `unchecked` -> skipped) |
| `spawns` | map | counts of `info_player_counterterrorist` / `info_player_terrorist`: both 0 fail, one 0 warn (fix `create_entities`) |
| `spawn_in_mover_path` | map | `func_movelinear` (`movedir` + `movedistance`) and `func_door` (extent along `movedir` minus `lip`) over their tied mesh bounds; the swept box is the union of start and end; a spawn whose player hull (32 x 32 x 72, strict overlap) meets it -> warn (fix `set_node_transform` to the nearest clear spot). `func_movelinear` honours `startposition` / `authoredposition` (the saved box may sit mid-travel, the sweep goes both ways from it). A mover that could not be swept (no tied mesh) and `func_tracktrain`, `func_train`, `func_rotating`, `func_door_rotating`, `func_platrot` make the check `skipped` (listed in `detail`) unless a hit already warns |
| `objectives` | map | bomb target / hostage rescue / hostage / buy zone counts; none at all -> warn (a blockout may have none) |
| `loose_compiled` | release | `_c` files under `game/csgo_addons/<addon>/` (except the map's own `maps/<name>/` output) the map vpk does not contain (a loose `maps/scripts/x.vjs_c` counts): warn when the map's keyvalues, mesh materials or the addon's text files name them (the Workshop upload must carry the addon folder); the others are listed as `looseOther`; no readable vpk or no references -> skipped |
| `debug_patterns` | release | regexes (built-in: `debug\s*[:=]\s*true`, `RegisterCheatCommand`, `sv_cheats`, plus `debugPatterns`) over the addon's text files (js/ts/json/cfg/txt/kv3/vjs/..., content and game side; `*.d.ts` and `tsconfig*.json` are skipped) and the `.vmap` string keyvalues: warn with `file:line` |
| `no_metamod_load` | release | always skipped: the tools session always loads Metamod, so this cannot be proven here; the manual step is in `fix.do` |
| `test_cvars` | release | with a map running (`game.local`), `cvar.get sv_cheats` (+ debugPatterns that look like cvar names, warn when set): 1 -> warn; a null/missing value, no game or no bridge -> skipped. A directory scan that hit its bound (20000 entries / 12 levels) can never pass (`scanTruncated`) |

So `release` is at best `incomplete` until `no_metamod_load` can be answered. `compile_map` and `run_map`
successes also suggest `report_ready` (`tools/mod.rs` `ready_next`, an `after_call` hook, so it needs the
full registry). Tests: unit tests in the module (verdict, mover geometry, counts, argument names) and
`tests/report_ready.rs` (temp tree, `probe.vmap`, fake VPKs from `tests/common`, a mock bridge for
the clean-document pass).

## Playbooks (`src/playbooks.rs`, streamline P4)

Six step-by-step procedures, written from what ti_convoy and the acceptance runs actually did:
`blockout_map {map}`, `iterate_loop {map}`, `cs_script {script}`, `particle_set {folder}`, `release {map}`,
`import_model {source, kind}` (I8; `kind` prop (default) | world | hammer_mesh | player picks the steps, an unknown
kind is `InvalidArguments` on `get_playbook`; player ends in a client-mcp session because Hammer's `run_map` does not show
the pawn; every kind carries the engine model cache rule).
Each is at most 2500 characters, ends with a definition of done (compile ok with 0 failed,
`report_ready` without a fail, a `look` described in writing) and the rule "if you had to step
outside these tools, say so and stop".

- **Two doors, one text.** MCP prompts (`prompts/list`, `prompts/get`; the transport is
  `cs2-mcp-common` `prompts.rs` + `mcp.rs`, advertised as the `prompts` capability only when a
  server attaches a `PromptSet`) and the read-only tool `get_playbook {name?, map?, script?, folder?}`
  for agents that only call tools (no name lists them; `source`/`kind` are import_model's). Both call `playbook_text`. The playbook
  text is not in tools/list; the tool costs 686 B there.
- **No invented paths.** Without a `map` argument the calls omit `path` / `map` (the tools default to
  the active document) and the text says "your map (the active document)"; only a path the caller
  passed is ever written.
- **Editing a playbook:** keep every tool name and argument real. `tests/playbooks.rs` parses each
  `tool {...}` snippet (bare keys, placeholders, `...` for "incomplete") and validates it against the
  tool's full input schema: unknown keys, required keys (e.g. `origin` in a create_entities item), types,
  enums, `look` frames needing a target or pose; every other snake_case word must be in a short allowlist
  (entity classes, events, step kinds).
  Add a new non-tool word to that allowlist deliberately.
- **`cs_script` uses the `cs_script` tool** (see "cs_script" below): init/add/write/build/compile
  replace the old "the user builds it" stop.

## Run-4 fixes (branch `streamline/run4-fixes`)

- `compile_map {path?, waitMs}` (no jobId) starts the job, then waits like `{jobId, waitMs}` (same
  `wait_for_job`, clamped to the call budget); the reply is a poll (`jobId`, `done`, `waitedMs`, the
  wait `next` while running) plus the start's `notes`/`orphanKills`. Every start refusal is unchanged;
  a bad `waitMs` is refused before anything starts. `cancel` without a jobId is still refused.
- `report_ready {mode?: "defuse"|"hostage"|"none"}`: `objectives` is informational (status `pass`, counts
  in `detail`, "pass mode:\"defuse\" to require them") for unknown/none. `defuse`: no bomb site = fail,
  no buy zone = warn. `hostage`: no hostage (`hostage_entity`/`info_hostage_spawn`) or no
  `func_hostage_rescue` = fail.
- `look {target:"map"}` frames on the saved mesh extents (`get_scene_info` now also returns `meshBounds`,
  null with no meshes) and falls back to entity origins only with no meshes; `target.framedOn` says which.

## Model facing (`src/tools/model_facing.rs`, P5)
`get_model_info` returns `facing {forward: "+x"|"-x"|"+y"|"-y"|"unknown", basis, confidence: high|medium|none, longAxis?, next?}` (ti_convoy FIXME 21). It is decided ONLY from attachments: names with a front or rear token (`front fl fr headlight(s) headlamp` / `rear back rl rr taillight(s) tail`, whole `_`-separated tokens) and their positions. `m_vInfluenceOffsets` are in the BONE's frame, so `AttachmentPoint::model_offset` accepts an offset as a model position only when the attachment has exactly one influence (`m_nInfluences == 1`), it is not root-transform relative, and the bone and every parent up to the root have identity rest rotation (`|w| >= 0.9999` of `DATA.m_modelSkeleton.m_boneRotParent`) and unit scale (`m_boneScaleParent`); otherwise that attachment casts no vote and `basis` says which rule failed (a 180-degree yawed bone must not give +x: tested). Accepted offsets are compared only on one shared bone: mean(front) minus mean(rear); the dominant of x/y must be at least twice the other and at least 1 unit. One agreeing bone = `medium`, two or more = `high`, a disagreement = `unknown`. No attachments, no front/rear names, different bones or no usable offsets = `unknown` with `basis` saying which, and `next` model_render from two sides. `longAxis` is the render box's longer horizontal axis, reported separately and never used for the sign. Measured on the real install: `car_a` and `utility_truck` have NO attachments (so `unknown`), and the only model in the pak with front/rear names (`passenger_train_4_position_01`) puts every wheelset on its own bone (and its root bone is yawed 90 degrees), so it is `unknown` too. Expect `unknown` for nearly all vanilla props; the field exists so a custom model with front/rear attachments is read instead of guessed.

## cs_script (`src/tools/cs_script.rs`, `src/cs_script/`, `templates/cs_script/`; bridge m3.17)

One tool `cs_script {action, name?, scripts?, file?, content?, waitMs?}` (Writer) keeps a TypeScript
project per addon at `content/csgo_addons/<addon>/cs_script/`; each script `src/<name>/index.ts`
builds with rollup to `content/csgo_addons/<addon>/maps/scripts/<name>.js` (the path a point_script's
`cs_script` keyvalue `maps/scripts/<name>.vjs` names). Based on Source2ZE `create-cs-script` 2.23.1 (MIT,
vendored under `templates/cs_script/`, licence copied into each project; local changes are listed in
`src/cs_script/mod.rs`). All writes pass `cs_script::inside_addon` (symlinks resolved, dangling links
refused); script names are `[a-z0-9_]+`; `write`/`read` touch only `.ts` files under `src/<name>/` of a
script that has a rollup target (the one way an agent authors a script through the MCP).

| action | does |
|---|---|
| `init {scripts?}` | refuses if `cs_script/package.json`+`rollup.config.js` exist (ProjectExists -> update/add). Scaffolds the template (OUTPUT_DIR = absolute host `maps/scripts`, package name `cs-script-<addon>`), runs `npm install @s2ze/*@latest` (create-cs-script's own install), then the types check. |
| `add {name}` | new `src/<name>/{index.ts,tsconfig.json}` and one line in the targets array between the `// cs-script:targets:begin/end` markers of rollup.config.js (markers gone -> ProjectConfig error, nothing written); duplicates and bad names refused. The tsconfig follows an active types override. |
| `write {name,file?,content}` / `read {name,file?}` | whole-file write/read of `.ts` (default `index.ts`, <= 256 KB); read without a file also lists the sources. |
| `update` | `npm install @s2ze/*@latest`, versions before/after, then the types check. `typescript` stays on the template's `^5.9` (TypeScript 7 has no JS API the check needs). |
| `build {name?}` | per script: `tsc --noEmit -p src/<name>/tsconfig.json --pretty false` (errors -> `failed[].errors[{file,line,col,code,message}]`, max 30, no bundle) then `rollup -c` with `CS_SCRIPT_ONLY=<name>` (noEmitOnError) -> the `.js`; reports `changed`/`existed`/`bytes` per output and a `reload` block. Missing node_modules triggers the install first. |
| `compile {name?}` | `.js` -> `.vjs_c` through `compile_asset {path:"maps/scripts/<name>.js", flags:["-nop4"]}` + `job_status` wait; reports the `.vjs_c` under `game/csgo_addons/<addon>/maps/scripts/`. `ScriptNotBuilt` if there is no `.js`. Needs bridge m3.17 (older: the refusal gets an "older than m3.17" hint). |
| `status` | project/installed, per script: sources, `built` (missing/stale/fresh by mtime of sources vs `.js`), `compiled` (`.js` vs `.vjs_c`), point_script entities of the open document whose `cs_script` keyvalue names it; installed `@s2ze/*`, latest `@s2ze/types` on npm (cached 10 min, 8 s timeout, `"unknown"` offline), types drift and override. |

Processes (`src/cs_script/run.rs`): `npm`/`node` from PATH (`CS_SCRIPT_NPM`/`CS_SCRIPT_NODE` override, the tests'
fakes), stdin closed, stdout/stderr captured, killed on their deadline. `npm install` is a **job**: when it
outlives the call, the reply is `state:"running"` and the same call again joins it (a second install is never
started). tsc/rollup run bounded by the call budget (`pending` lists scripts it did not reach).

**Types follow the game** (`src/cs_script/types.rs`): the game's `point_script.d.ts`
(`<app>/content/csgo_addons/cs_script_demo/maps/scripts/`) is compared with the npm package's built
`autoimport/index.d.ts` by `templates/cs_script/api-fingerprint.mjs` (node + the project's `typescript`: exports,
members, signatures; formatting and member order do not count; 2.23.1 == game, 2.23.3 differs: it adds
`Console`/`GameRules`/hostage API the game's file lacks). Different -> `types/point_script.d.ts` (copy of the
game's) and every `src/*/tsconfig.json` gets `compilerOptions.types: []` + `include: [../../types/point_script.d.ts,
./**/*]`, so the npm autoimport declaration is not loaded and the game's file is the only declaration of the
ambient module (a `paths` mapping cannot win: ambient module declarations are resolved first). Equal again ->
the file and the keys are removed. Unknown (game file or node missing) -> nothing touched. Proved by the real
test `real_npm_roundtrip` (`Console` is rejected with the override active).

**Hot reload sign (unverified live).** `build` watches console.log (`ConsoleWatch`, up to 4 s, only when the
bridge says a map runs) for: `[<name>] loaded` (printed by the template at the top level of each script; the
engine re-runs the top level on reload, point_script.d.ts "Tools Mode"), the engine's own `Loading script "%s"` /
`Failed to compile cs_script[%d]` / `Failed to load cs_script "%s"` (strings in server.dll). `reloaded:true` on
the first two, `false` on the failure strings (`evidence` is the line), else `unknown`. There is no verified reload
console command: `script_reload` in server.dll is the VScript command ("Scripting disabled or no server running"),
so it is NOT offered.

Tests: `tests/cs_script.rs` (fake npm/node; `--ignored` runs the real toolchain and the network:
`cargo test -p hammer-mcp --test cs_script -- --ignored --nocapture --test-threads=1`).

## Model import overview (I0-I8)

Which tool for which job (details in the sections named; open gaps are logged in `workflows/import/FIXME.md`, the
state of every work package in `workflows/import/STATUS.md`). Or follow the playbook `get_playbook {name:"import_model"}`.

| Job | Tool | Section |
|---|---|---|
| model file -> a placeable model (`kind` prop / world; skinned glTF = rigged prop with its clips) | `model_import` | `## model_import`, "Skinned glTF" |
| model file -> editable Hammer meshes in the open map | `model_import kind:"hammer_mesh"` | "`kind:hammer_mesh`"; `align_texture mode:"axes"` |
| humanoid -> a CS2 player skin | `model_import kind:"player"` | "`kind:player`" |
| check / place the result | `get_model_info`, `model_render`, `create_props` (all take `models/**` and `agents/models/**`) | `## Model facing` |
| make the real pawn wear it | `player_model {action}` (cs_script route; the bridge route is deferred) | `## player_model` |
| reuse a group of nodes | `prefab {create, instance, list, read}` | `## prefab` |
| see it | `look {target:<nodeID, prefab nodeID, ...>}` | `## look` |

- **Facing**: a CS2 model faces **+X**. `model_import` turns a glTF (+Z front) for you; FBX/OBJ/SMD are used as
  authored. `get_model_info.facing` is `unknown` for most models; compare with a shipped model in `look`.
- **Engine model cache**: a running engine keeps a `.vmdl_c` it already loaded, even across `run_map`. After
  re-importing or recompiling a model, restart the tools session and recompile the map (it embeds models) before judging
  a look. `model_import`'s `look` step and `model_compile`'s `notes`/`next` say so (one text,
  `model_file::ENGINE_CACHE_ADVICE`).
- **Precache**: a model the map does not list is "requested but not in the system" and a pawn shows the ERROR model;
  `player_model set` adds a hidden `prop_dynamic` of it to the map for that reason. `run_map` in Hammer loads models on
  demand, which hides the problem: verify a player model in a client-mcp `launch_session {kind:"wine-tools"}`.
- Nothing here saves: `save_map`, then `compile_map`, then `run_map` (`run_map {unload:true}` first when loaded).

## model_import (`src/tools/model_import.rs`, `src/import/`, I1)

Writer. `source` (host path or inside the addon) -> `models/<name>/<name>.vmdl` + mesh + materials in the
active addon, compiled unless `compile:false`. `kind` `prop` (hull collision) | `world` (concave
`PhysicsMeshFile`); `hammer_mesh` (I3, below); `player` (I6, below) retargets a skinned humanoid onto the CS2 player skeleton. `dryRun` returns the
plan and writes nothing; an existing `.vmdl` needs `overwrite` (`ModelExists`); other replaced files are backed up
like `model_create`'s.

- **glTF/GLB** (`gltf_read.rs`, the `gltf` crate with `import` off: no `image`, buffers/`data:` URIs are resolved
  by us, only relative files without `..`) -> `Scene` (node world transforms applied) -> `smd.rs` -> one SMD used
  for render and physics. A file with a skin is the skinned path (I5, below); without one, animations are
  dropped (reported). COLOR_n / TEXCOORD_n>0 / morphs / point-line
  primitives / emissive / occlusion / alpha mode land in `dropped`. Required extensions outside
  `KHR_materials_*`, `KHR_texture_transform`, `KHR_lights_punctual` -> `UnsupportedExtension`. More than 500 000
  triangles -> `TooLarge`.
- **Axes/scale/V** (`smd.rs`): `(x,y,z)_gltf -> (z,x,y)_cs2` (a rotation: +Y up -> +Z up, front +Z -> +X, +X ->
  +Y; winding kept), metres -> 39.37 units, `v = 1 - v`. `upAxis:"z"` = identity. Tests pin it with an asymmetric
  triangle.
- **Materials** (`materials.rs`): `materials/models/<name>/<mat>.vmat` (`csgo_complex.vfx`, `TextureColor`,
  `TextureNormal`, `TextureRoughness`, `TextureMetalness`, `g_vColorTint`; all `[I]`) + the PNG/JPEG bytes beside
  it; the vmdl's `DefaultMaterialGroup.remaps` maps each SMD material (`<mat>.vmat`) to its vmat
  (`vmdl::material_remap`, the one place for the key spelling). A packed metallicRoughness texture feeds both
  keys unsplit (warning). `materials:"none"` -> `use_global_default` with `materials/dev/reflectivity_50.vmat`.
- **`materials:"existing"` (I2)** writes no `.vmat`/texture: `materialMap {<source material>: "materials/x.vmat"}` is
  turned into the same `remaps`. Every target must resolve (`asset_resolve::Sources`: addon game dir, game dirs,
  game paks; a `.vmat` or `.vmat_c` spelling; an addon `.vmat` source with no `_c` is accepted with a
  "compile_asset it" warning) else `UnknownMaterial{unknown, didYouMean}` (same-folder names, nearest first) and
  nothing is written. glTF source names = `materialNames` in every result (dryRun lists them): glTF material names
  sanitized and made unique by `import::materials::smd_names`; a key is matched exactly or after `sanitize`, so the
  raw glTF name works too. A material missing from the map uses `materials/dev/reflectivity_50.vmat` with a
  warning; a key matching nothing warns. Pass-through formats: the map is passed as given (key minus `.vmat`),
  warning that names are not checked (the file is never read). `materialMap` without `existing` is
  `InvalidArguments`. The result's `remapped` is `{name: target}`; `materials[]` stays empty.
- **fbx/smd/obj/dmx** are copied under their own name and referenced (`import_scale = scale`, default 1); their
  materials stay theirs.
- The compile is the shared `compile_resourcecompiler` (`ANY_TOOLS`); a failed compile is `CompileFailed
  {failLines}` with `useInstead: model_compile` and the files kept. `next`: `get_model_info` -> `create_props` ->
  `save_map` -> `look` (custom props need an explicit `pose`) -> `report_ready`.

Tests: `tests/model_import.rs` (fixtures from `tests/common/gltf_fixture.rs`, fake podman), `tests/model_import_rig.rs`
(I5, pure). Hacks and open checks: `workflows/import/FIXME.md`.

### Skinned glTF (I5: `import/{gltf_rig,skeleton,mat}.rs`, `smd::write_rigged`)

A glTF with a skin becomes a `skeletal` prop (`prop_dynamic`): reference SMD with the real skeleton and per-vertex
links, one animation SMD per glTF animation (`models/<name>/<sequence>.smd`, 30 fps) listed as `AnimFile`s. The math
is in the module docs of `gltf_rig.rs` (what is read, sampled) and `skeleton.rs` (SMD frames): bind world = `inverse(IBM)`,
vertices as given (skin space), `W_j(t)` = the joint's global node transform (rest TRS where a channel is missing,
non-joint ancestors baked), SMD local = `inverse(W_parent) * W_j`, one basis change `(x,y,z)->(z,x,y)` + import scale
(translations only) + the Rz(-90) pre-turn applied identically to the bind skeleton, vertices/normals and every frame,
Euler gimbal-safe (matrix route, never quaternion-to-Euler).

- **Reads.** The first skin only (more: warning + `dropped`); parent = nearest joint ancestor, several roots get a
  synthetic `root` bone; `JOINTS_0/WEIGHTS_0` normalised, weights < 0.001 pruned, the strongest 4 kept, renormalised
  (`JOINTS_1/WEIGHTS_1` dropped, listed); a vertex without weights goes to the root bone (counted in a warning).
  Unskinned meshes ride their nearest joint ancestor (else the root bone), warning names them. Animations:
  translation/rotation/scale, LINEAR (slerp) and STEP; CUBICSPLINE uses the key values (warning); morph `weights`
  channels, channels on nodes outside the skeleton: dropped. Frames = `round(duration*30)+1` from the first key to the
  last (0-length or empty clip = 1 frame, warning when empty); more than 2M frames x bones is refused.
- **Scale.** SMD has no scale: a joint scale equal to its bind scale and uniform is exact (the rigid frames reproduce
  the skinning); anything else keeps rotation + translation and is listed in `dropped`.
- **Bone names (policy).** Lowercased (the SMD importer does it), `"`/control chars -> `_`, empty -> `bone_<joint>`;
  a name already taken gets `_2`, `_3` (the later joint in `skin.joints` order; the synthetic root is named last, so
  it yields to a real `root`), warning. Bones are ordered parents first, depth first. The result lists both names.
- **Sequences.** Name = sanitised animation name (`anim_<index>` when it has none), unique (`_2`...) and never the
  model stem (the stem's `.smd` is the mesh). `animFiles` order = file order.
- **Collision** is one rigid hull (or none) of the bind pose; it does not follow the bones (warning). `kind:"world"` and
  `physics:"mesh"` with a skin are `InvalidArguments`; `hammer_mesh` drops skin/animations (reported). `kind:"player"`
  is I6 (below). fbx/smd/obj/dmx animations are passed through untouched (nothing reads or converts them).
- **Result.** `skeleton {bones, root, names:[{source, smd}]}` (`source` null = the synthetic root), `animations
  [{name, frames, fps}]`; `next` places `create_props {class:"prop_dynamic", keyValues:{StartingAnim:<first clip>,
  StartingAnimationLoopMode:"ANIM_LOOP_MODE_LOOPING"}}` and ends with `look {mode:"frames"}`.

### `kind:"player"` (I6: `src/import/retarget/{skeleton,mapping,bake}.rs`)

A skinned humanoid glTF/GLB of ANY rig becomes a model on the CS2 player skeleton, posed by the game's own AnimGraph2
(`prop_dynamic` plays it; the real pawn: `player_model`, below). Extra args: `boneMap {sourceJoint: cs2Bone}`, `rig`
`auto|mixamo|rigify|ue|vrm` (default auto), `scale` (CS2 units per source unit; default = fit, reported),
`referenceAgent` (default `agents/models/ctm_sas/ctm_sas.vmdl`; below). `upAxis` is refused (the orientation is derived
from the rig), as is `agent` (the dispatcher's unknown-argument refusal: the skeleton and graph are the same for every
agent). Source animations are dropped (noted); `physics` defaults to none (`hull` allowed, `mesh` refused). Output under `models/<name>/`: the SMD (skeleton = the vnmskel verbatim, 75 bones,
mesh in vnmskel model space, both with the prop path's Rz(-90) pre-turn on the root bones/vertices/normals so the
compiled bind equals the vnmskel reference pose exactly, as Hammer's viewport shows it beside a real agent) and a
`.vmdl` with `NmSkeletonList>NmSkeletonReference{animation/skeletons/characters/worldmodel.vnmskel}` +
`AnimGraph2List>DefaultAnimGraph2{animation/graphs/worldmodel/worldmodel.vnmgraph}` (the I0 spike's layout) plus the same
graph as `AnimGraph2{name:"worldmodel"}` (the identifier the player pawn asks for, as every agent carries it: without it a
pawn wearing the model stood in the bind pose with its weapon floating at the `wpn` rest point, live 2026-11-03; a
`prop_dynamic` uses the default one). The skeleton
is read from the game paks (`worldmodel.vnmskel_c`; `SkeletonNotFound{path}` when absent). `get_model_info
{model:"animation/skeletons/characters/worldmodel.vnmskel"}` dumps it (bones, parents, both poses); `include:["skeleton"]`
lists any model's bones; `agents/models/**` models resolve everywhere a `models/**` path does (catalog, `get_model_info`,
`create_props`; `list_models` lists them, prefix `agents/models/`).

- **Mapping** (`mapping.rs`): explicit `boneMap` (names validated on both sides, case-insensitive, `InvalidArguments` +
  `didYouMean`; `wpn*`/`attach*`/`camera`/`root_motion` and two joints on one bone refused) > name tables > structural
  (core set only, and only for roles the tables left open). Required: pelvis, at least one spine, neck_0, head_0,
  clavicle/arm_upper/arm_lower/hand L+R, leg_upper/leg_lower/ankle L+R (18 roles) else
  `BoneMapIncomplete{unmapped, required}` with a `boneMap` hint. Optional: ball, fingers (thumb 0..2; the others
  meta,0..2, aligned to the tip). Tables (`mixamo`, `rigify` DEF-*, `ue` mannequin, `vrm`) match canonical names
  (prefix up to `:`/`|`, `mixamorig<n>`, `DEF-`, separators, side token or `left`/`right` prefix dropped);
  spine joints are spread over spine_0..3 by even spacing (n=3 -> 0,2,3). Structural: pelvis = shallowest joint
  with a trunk subtree >= 2x each of two similar leg subtrees; the spine follows single children to the first
  branching joint; neck = the child most along up, arms = the two biggest other subtrees, legs the two small
  ones; left = `up x Z` (glTF front +Z). Result `skeleton {bones, mapped[{source, cs2, how}], unmapped[{bone,
  weightsMovedTo}], unmappedUnweighted, confidence}` (confidence = required roles found by name/explicit 1, structural
  0.7, over 18).
- **Math** (`bake.rs` module docs hold all of it): axes up = head_0 - pelvis, left = clavicle_L - clavicle_R
  (orthogonalised), forward = left x up, derived on BOTH the CS2 skeleton (+Y left, +Z up, +X forward within 3 deg) and
  the mapped source joints; rotation R between the frames, scale k = pelvis-to-head ratio; then per CS2 bone
  `v' = pc + Q (S (v - ps))` with Q taking the source bone direction (to the next mapped bone of the chain) onto the CS2
  direction, roll fixed by a shared global reference axis, S stretching only the segment itself to the CS2 length
  (clamped 0.1..10; behind the joint rigid, past the successor carried along); terminal bones (head, tips, ball) use
  their CS2 frame's +X and the source child closest to it (over 45 deg off: the predecessor's Q; the pelvis without a
  successor: Q = I). Near-coincident joints are not directions. Linear blend, normals by the inverse transpose of the
  stretch. Weights of unmapped joints move to the nearest mapped ancestor (else pelvis). A mapping whose chain runs up
  the source hierarchy (a `boneMap` slip) is refused as `BoneMap`.
- **Weapon attachments + hitboxes** (`retarget/agent.rs`): copied from `referenceAgent`'s compiled `.vmdl_c` (game paks
  or loose files; `AgentNotFound`/`AgentRead`, an agent with neither = `InvalidArguments`): every `MDAT.m_attachments`
  entry becomes an `Attachment` (`parent_bone`, `relative_origin`, `relative_angles` = the quaternion as Source QAngle,
  `weight`, `ignore_rotation`; `weapon_center` is root-relative, `parent_bone ""`), every `m_hitboxsets` set a `HitboxSet`
  of `HitboxCapsule`s (`radius`, `point0`/`point1` = the compiled min/max, `surface_property`, `group_id`,
  `translation_only`). Bone names lowercase (the compiled import's bones are). Offsets are bone-relative and copied
  verbatim: below the root every agent bone shared with the vnmskel has its parent-space rest pose (measured), and the
  retarget gives each bone its CS2 length, so the Rz(-90) pre-turn (root bones/mesh only) does not touch them. An
  attachment on an agent-only bone (`primary` on `jiggle_primary`) is folded into the nearest vnmskel ancestor
  (`spine_3`) through the agent's rest pose (`markup.moved`); anything with no such ancestor is dropped and named
  (`markup.dropped` + a warning; a fold may not end on the root bone, whose agent rest frame is turned against the
  vnmskel's). Why a reference agent and not a table: holster offsets differ per agent (CT agents carry
  17 names incl. `defusekit`/`primary_smg`, T agents 15), and reading the pak follows game updates. Hitboxes are NOT
  scaled: they are identical in all 80 shipped agents whatever the body (set `cstrike`, 19 capsules, groups 1 head, 2
  chest, 3 stomach, 4/5 left/right arm, 6/7 left/right leg, 8 neck), i.e. the game keeps hit areas equal across skins, and
  the import's bones have the CS2 lengths, so an unusually bulky or thin mesh gets the standard hit areas, not its own
  silhouette. `referenceAgent:"none"` writes no markup. Every vnmskel bone also gets a `BoneMarkup {target_bone, do_not_discard:
  true}` (lowercase): without it the compiler drops every unweighted bone no attachment/hitbox names (the I7 box humanoid
  compiled to 20 bones, no `root_motion`/`wpnPivot`/`wpn`); `do_not_discard_skeleton_bones` on the `NmSkeletonReference`
  does NOT work here (the compiler then tries to compile the pak's `worldmodel.vnmskel` from a source and fails). And the
  `RenderMeshFile` gets one `RenameBone {original_bone_name, new_bone_name}` per mixed-case vnmskel bone (65): the SMD
  importer lowercases bone names, the renames give the compiled model the vnmskel's (and the agents') exact names.
  Attachment/hitbox/BoneMarkup bone lookups are case-insensitive (the lowercase names resolve to the renamed bones).
  Verified live 2026-11-03 (build 2000940): the compiled import has 75 bones, 17 attachments, the `cstrike` set of 19
  capsules; the CT pawn wearing it holds its pistol in both hands and `ent_hitbox player` draws the capsules on the
  posed bones. Result `markup {referenceAgent, attachments[names], hitboxSets[{name, hitboxes}], moved[{attachment,
  from, to}], dropped}`. Inferred/verified status: FIXME.md "Attachments".
- **Result** adds `retarget {scale, scaleFrom, rig, facing{space, cs2{up,left,forward}, source{...}}}`; `next`:
  `get_model_info`, `create_props {class:"prop_dynamic", model, keyValues:{}}` (`use_animgraph` defaults to on),
  `player_model {action:"set", model}` (the real pawn), `save_map`, `look {mode:"frames"}`.
- Tests: `tests/model_import_player.rs` (synthetic `.vnmskel_c` from `tests/common/vnmskel_data.rs`, the procedural
  Mixamo humanoid `tests/common/humanoid.rs`, tables, structural, math, tool); one `#[ignore]` test reads the real
  skeleton from the install. `tests/model_import_markup.rs` (synthetic agent `.vmdl_c` from `tests/common/agent_data.rs`,
  numbers of the real `ctm_sas`: read, fold, angles, vmdl nodes, template refusals); its `#[ignore]` tests compare the
  installed `ctm_sas` with the fixture and every installed agent's hitboxes with it.

### `kind:"hammer_mesh"` (I3: `src/tools/hammer_mesh.rs`, `src/import/{planar,chunk,hammer_mesh}.rs`)

A glTF/GLB as Hammer meshes in the OPEN map (bridge m3.18; FBX/OBJ/SMD/DMX are `UnsupportedFormat`: they are props).
Extra args: `origin` [x,y,z] and `angles` [pitch,yaw,roll] (default 0; applied to the vertices before the axes
are computed), `maxTrisPerMesh` (default 2000, 30..=20 000: a merged face holds up to 30 triangles and is never split); `scale`, `upAxis`, `materials`/`materialMap`,
`compile`, `name` (the folder of the generated `.vmat`s, `materials/models/<name>/`) work as for props;
`physics` is refused (meshes collide as they are); an existing generated file (e.g. a prop import of the same
`name`) is `ModelExists` unless `overwrite:true` (then replaced with a backup). `dryRun` plans (chunks, `facesPerChunk`, files) with no bridge call at all.

- **Flow.** Gate first: `mesh.create` and `mesh.setFaceTexture` advertised and `bridgeVersion` >= m3.18
  (`NotImplemented{missing, needs:"m3.18"}`, nothing written). Then write the materials and compile them
  (`compile_resourcecompiler_set`; `CompileFailed` before anything is in the map). Then per chunk, under the write
  gate: `mesh.create` (the `create_mesh` pipeline: 4-decimal rounding, size gate), `mesh.get` to pair the faces
  sent with Hammer's face indices (same corners in any order, same winding, nearest centroid; creation order assumed, with a warning, when the bridge has no
  `mesh.get` or the mesh is too large for one reply), then ONE `mesh.setFaceTexture` `axes` call. A face Hammer
  dropped/welded and that cannot be paired stays at the default texture (warning).
- **Result.** `{nodeIDs, chunks, facesPerChunk, trisPerChunk, uvMode:"axes"|"default", uvExact, materials,
  materialNames, mesh:{tris, faces, verts}, dropped, warnings, files, compile, origin, angles, scale, saved:false,
  undoNote}`; `next`: `look {target:<first nodeID>}`, `save_map`, `compile_map` (`run_map {unload:true}` first if
  the map is loaded in game), `run_map`. `uvMode:"default"` = no face had usable UVs (none in the file).
- **Faces.** Triangles become faces; coplanar neighbours merge only into CONVEX polygons of the same material
  and the same UV mapping (otherwise they stay triangles); the texture axes are solved per face from its UVs (exact
  at every corner, rotated/sheared/mirrored included); the formula and tolerances are in `import/planar.rs`.
  Unusable UVs get Hammer's default world projection (counted in `warnings`). The texture size comes from the
  generated base-colour PNG/JPEG header; for `existing`/`none` materials or an unreadable image it is assumed
  512x512 and said in `warnings`.
- **Failure part-way.** `ChunkFailed{created:[nodeID..], failedChunk, chunks, stage:"create"|"pair faces"|"align",
  cause}` with `useInstead: delete_nodes {nodeIDs: created}`. A chunk is TWO undo steps (create, align):
  `undo_last` twice removes the last chunk (a chunk with nothing aligned is one step; `undoNote` names it); an unsaved document needs no cleanup beyond that. There is no resume:
  delete the created nodes and run again.
- Tests: `tests/hammer_mesh_planar.rs` (planarity, convexity, axes vs UVs, chunk limits), `tests/hammer_mesh.rs`
  (mock bridge: call order, partial failure, dryRun, gating), `tests/align_texture.rs` (axes mode).

## player_model (I7, `src/tools/player_model.rs`; cs_script route)

`player_model {action:"set"|"clear"|"status", model?, team?:"any"|"ct"|"t", bots?}` (Writer) makes the real player
pawn wear an imported model (e.g. `model_import kind:"player"` output). No bridge change: the game's own script API
does it. Spike, verified live 2026-11-03 (`workflows/import/FIXME.md` "I7 spike"): `Instance.OnPlayerReset` ->
`QueueAfterThinks` -> `pawn.SetModel(model)` keeps AnimGraph2 driving the pawn; `OnPlayerReset` fires on respawn,
team change and round restart. The tool itself was run live the same day (`any` slot, bots true: the CT pawn wears the import; `ct`/`t` slots and
`bots:false` unproven; FIXME.md "I7").

- **`set {model, team="any", bots?}`**: one rule per slot (`any`, `ct`, `t`; `ct`/`t` win over `any`; team numbers 3 =
  CT, 2 = T; spectators are left alone). `bots` is one flag for the whole script (default true; omitted keeps the
  stored value). Steps, each reported in the reply (`add`, `write`, `build`, `compile`, `map`):
  1. the model goes through the shared resolver (`get_model_info`/`create_props` normaliser: `models/**` and
     `agents/models/**`, `.vmdl_c` accepted) and must be compiled in the catalog: `ModelNotFound` (next model_import),
     `ModelNotCompiled` (next model_compile), `InvalidParams` for a bad path (only `a-z0-9_-./`).
  2. the cs_script project must exist: `NoProject` with next `cs_script init` (no implicit npm install; a project
     with no `node_modules` is installed by `cs_script build` itself).
  3. `cs_script/src/player_model/index.ts` is rendered from ONE fixed template (`render_source`); the first line is a
     marker and the rules sit in one JSON const (`const RULES: Rules = {...};` after `// @player_model:rules`), so
     `status` and the next `set` read them back (`parse_source`). A file without the marker is never overwritten
     (`ScriptNotOurs`). Then `cs_script` `add` (only when the target is missing), `write`, `build`, `compile`: the
     same functions the `cs_script` tool runs. A build/compile that fails or is still running stops the call with
     `stage`, the step's report and its `next`; repeat the call (idempotent).
  4. the open map (live bridge, through `find_entities_live`/`get_entity_live`/`create_entities`/
     `set_entity_keyvalues`/`delete_nodes`): exactly one `point_script` `targetname player_model_script` with
     `cs_script maps/scripts/player_model.vjs` (created at [0,0,0], or an existing one keeps its origin and gets its
     keyvalue fixed; duplicates of ours are deleted), and one `prop_dynamic` `player_model_precache_<k>` per distinct
     model (k = order any, ct, t) with `model` plus `solid 0`, `rendermode kRenderNone`, `disableshadows 1` (new
     props sit at the script's origin; an existing one has drifted keyvalues fixed, is not moved). Props of ours that
     no rule uses are deleted. Entities with any other targetname are never touched. No open map: the script half is
     done, the reply is `ok:false stage:"map"` with `next` open_map, then repeat.
- **Why the precache props**: a model that is not in the map's resource manifest is "requested but not in the system"
  and the pawn shows the ERROR model (verified live). A `prop_dynamic` of the model in the map makes the map compiler
  list it. The hide keyvalues come from the real `core/base.fgd` (`solid` 0 "Not Solid", `rendermode` kRenderNone
  "Dont Render", `disableshadows` 1); invisibility was seen live, non-solidity was not probed (FIXME.md "I7"). The bridge route (`game.setPlayerModel`) is deferred.
- **Failure handling**: the map sync finds its entities by exact targetname (no class scan, so the bridge's 2000-row cut cannot hide them) and, if it stops part way, replies `map {synced:false, created, createdNodes, fixed, deleted, error}`; `open_map` is suggested only for a missing document, otherwise repeat the call (it converges by targetname). A marked `index.ts` whose rules const is unreadable is `ScriptDamaged` in `status` and rewritten by `set`/`clear`; a marked file with no rollup target gets just the target re-inserted.
- **`clear {team?}`** drops one slot (all without `team`), regenerates the script and syncs the same way; with no rule
  left the point_script and the precache props are deleted FIRST (the generated script stays, with empty rules; its write/build/compile run afterwards as best effort under `scriptSteps`). Not seen: a point_script renamed away from `player_model_script`; with `bots:false` a pawn without a controller counts as human.
- **`status`** reads the rules from the generated file, `built`/`compiled` (mtime based, like `cs_script status`,
  without its npm lookups) and, when a map is open, `map {inSync, pointScripts, precacheProps, missingPrecache}`.
- **Nothing auto-saves**: `next` is `save_map` -> `compile_map`. Verification is NOT Hammer's `run_map` (it loads
  models on demand, which hides the precache error, and the pawn stays at the origin). Verify with the other MCP server,
  client-mcp: `launch_session {kind:"wine-tools", addon, map}`, `console_exec "sv_cheats 1; thirdperson"`, `screenshot`.
- Tests: `tests/player_model.rs` (fake npm/node, mock job bridge, mock Hammer world: golden source, set/idempotence,
  slots, sync fixes + foreign entities untouched, model change / clear, status round trip, refusals, no open map) and
  the unit tests in the module; budget in `tests/tools_list_budget.rs`.
