# 03 — Hammer tools: port the POC (24 tools)

Status: entities, lifecycle, file reads, mesh tools, **renders (WP-D)**, **run (WP-R)** and
**`compile_map` (WP-C)** are done; no documented stubs remain.

## Goal
Every tool in `../idabins/worknotes/mcpserver/README.md` "Tools" exists in Rust with the same
name, arguments, policies and error shapes, passing the ported contract tests and the live
acceptance scripts' checks.

## Prerequisites
- 00 (server + bridge client + mock) and 01 (runtime) done; a live session with
  `maps/probe.vmap` in addon `mcp_test` for acceptance.
- Read `../idabins/docs/flows/hammer-entity-edit.md`, `hammer-mesh-edit.md` §I, `hammer-undo.md`,
  `hammer-save-compile.md`, `hammer-view-render.md`, `map-preview-freecam.md` §F,
  `file-edit-reload-compile.md`; POC `server.py` and `geometry.py`.

## Steps (cluster by bridge dependency)
1. **Session/document** (`control.*`, `doc.active`): `session_status` (00), Windows->host path map
   (`hammer-mcp/src/paths`, POC `windows_to_host`: `Z:\app`, `Z:\cache`, `C:`, content-root fallback,
   case-insensitive resolve, `PathNotMapped`).
2. **Lifecycle**: `save_map` (`map.save`; the only writer; refuses `editable:false`/read-only host
   file; clears the unsaved counter), `reload_map` (`map.reload`; dirty/unknown -> `UnsavedChanges`
   unless `force`), `open_map` (`map.open` — **done in m2.16**: `CBaseEditorApp::OpenFile` flags 1,
   gated on the canonical path shape, the host file (`-32014`), the file's binary-v9 DMX header
   (the tool parses it with its own reader first: `InvalidMapFile`) and an already-open session that
   answers `existing:true` instead of leaking a second session; the tool and the bridge take no
   `force`. `file.open_asset` is *not* an open/reload primitive, see
   `file-edit-reload-compile.md` §B),
   `run_map`
   (`map.run`/`job.status` are **`-32011`**: ship gated; the engine load is reached over the
   VToolMessage `command` channel instead — **done (WP-R)**, below),
   **`run_map` (WP-R)** — `src/tools/run_map.rs`, the proven recipe (`HANDOFF.md` "Playtest",
   `pt_lib.py` `vcmd`, `compile_map.sh`), **not** `map.run`: one VToolMessage `command` payload per
   line (`vtmsend.exe`; `runtime.sh`'s own channel route, so it works without `-testscript`, where
   the bridge's `console.exec` answers `-32011`), each optional `game_type N`/`game_mode M` as its
   own post (the recipe's order; whether the channel splits on `;` is unverified) and then
   `map <name> nomapvalidation=1` (a plain `map <name>` fails
   "invalid map name"; `maps` is never sent — it crashes the engine). The name comes from `path`
   (the canonical `maps/…vmap` gate `open_map` uses, shared in `lifecycle::map_content_path`) or
   from the active document, and is validated against `[A-Za-z0-9_/-]` before it becomes a console
   argument. The **compiled** `.vpk` (`<app>/game/csgo_addons/<addon>/maps/<x>.vpk`, or the
   flattened `maps/<stem>.vpk` of the recipe, accepted for a sub-directory map only when it is the
   only candidate, newer than that map's `.vmap`, AND no top-level `maps/<stem>.vmap` SOURCE exists
   — a fresh top-level compile of the same stem is the unrelated vpk a newer-than-vmap check alone
   would accept (review-2 M8; `vpk.form` + `data.flattenedRejected`) must exist: a missing one is `VpkNotFound` before
   anything is sent (no Hammer compile dialog is ever involved); a `.vpk` older than its `.vmap` is
   `staleCompile`. A dirty document is **not** refused, only noted. **A map the engine already has
   mounted is unmounted first** (`remount`): the engine keeps serving the copy it loaded when the
   `.vpk` is recompiled under it, so the tool posts `disconnect` — the proven cure (`HANDOFF.md`
   "Playtest" item 1, playtest 2's `step4_reload.py`) — and waits for the idle level
   (`Spawn Server: <empty>`, 120 s, clamped to the budget) before the map command, with
   `game.local` (when exposed; the live `-32024 NoLocalPlayer` answer — the engine in NO map at
   all — reads as `inGame:false`, not unknown) or this server's last successful load deciding
   whether a map is up;
   a `disconnect` whose idle level never appears leaves the map command unsent (`failed`,
   `remount.failed`). **All of this is decided under the vtm_gate with FRESH inputs (review-2
   N2):** a `game.local` answer or a console offset taken before the gate wait can be STALE (a
   concurrent call of this server may have posted a whole load in between), so the gate holder
   re-reads `game.local` inside the gate and matches the idle line only after a FRESH offset
   taken right before its own `disconnect`. **`unload: true`** is that disconnect as a tool of its own (the primitive
   `compile_map`'s map-in-game refusal points at): `path`/`gameType`/`gameMode` are refused with it
   (only `disconnect` is sent, through the same gate), `timeoutMs` bounds the idle-level wait, and
   the result is `state ok|failed|timeout` (`failed` = the post did not come back — `delivered:false`,
   never repeated; `timeout` = posted, no idle line) or a no-op (`noop:true`, `state:"ok"`, nothing
   sent) when `game.local` says the engine is not in a map — where an UNRESOLVED record is
   settled first and KEPT unless it settles (review-2 N9: `game.local` can answer inGame=false
   while a load is in a non-frozen phase); a confirmed unload clears this server's
   own load memory, so the next `run_map` does not disconnect for a map that is not up. The verdict is
   the engine's own
   console line: the tool takes `game/csgo/console.log`'s offset (fresh, under the gate, N2) and polls it
   (250 ms) for `Loading map "<name>"` and then `OnSwitchLoopModeFinished( game : success )` (or a
   failure line) until `timeoutMs` (at most 600 s, always within the tool budget: the default is
   `min(60 s, what is left)` and a default cut that way is SILENT — the result's `timeoutMs` says
   what was watched — while only an explicit `timeoutMs` the budget had to cut reports
   `budgetClamped` + the note; the `disconnect` wait's budget note follows the same rule); the
   success (and every failure but the engine's own name validator) only counts **after** that
   `Loading map "<name>"` line, so a leftover success from an earlier load cannot answer for this
   one (a `game.local` naming another map gives `unconfirmed`; whether the engine spells a
   sub-directory map `sub/x` or by its stem is still unverified live, so that check can also read
   `unconfirmed` for a genuine sub map — the orchestrator's live pass settles it). One load at a time: a call whose
   engine showed no verdict records it, and the next call settles it in the console (or
   `game.local`) and refuses `EngineBusy` while it is unresolved; the record is written only once
   a `map` post (or a `disconnect` that can be followed by nothing) has gone out, and a post that
   FAILED before delivery leaves NO record and releases the external grace (review-2 N3: no more
   EngineBusy lockout of run_map AND the unload recovery primitive for the record window); a
   post that may have been
   delivered is never repeated (a `podman exec` timeout is `delivered:false` and makes an undecided
   load `state:"unknown"`), and the single fixed payload file is serialized by
   `HammerState::vtm_gate`. All outcomes are **results** (`state: ok|failed|timeout|unknown|unconfirmed`,
   `ok:false` for all but `ok`) with the
   console tail; `game.local` is added when the bridge exposes it (read with what is left of the
   budget), and `note_external_job` marks the load so
   the blocked gate says `EngineBusy` instead of `HammerBlocked` (its own grace field, review-2
   N4: `clear_external_job` releases only this window, and the `EngineBusy` data carries
   `externalJob` + the remaining grace). It takes no write gate and sends
   no `expectDoc` (it changes no document state). **The Hammer document while the map runs:** the
   defuse playtest loaded the map with its document open and read the same `doc.active` (`epoch:1`,
   `undoDepth:48`, `dirty:false`) before and after [V, `HANDOFF.md` §Playtest steps 1 and 6] — the
   load does not touch it. What a bridge request sees *during* the load (frozen frame loop) is
   **unverified live: the orchestrator verifies**.
   `compile_map` (**done, WP-C branch `hammer/cmp-a`**, `src/tools/compile_map.rs`): the headless
   direct `resourcecompiler` route of `~/.tmp/defuse-poc/compile_map.sh` + HANDOFF.md "Compile +
   playtest (worker)", never the dialog. No `-game` (unlike the model/particle file routes — the
   recipe proven live leaves it out). The command exports `CS2MCP_COMPILE_JOB=<tag>` — the tag
   VALUE unique per process (`<jobId>-<pid>-<nonce>`, N1: two hammer-mcp processes can both run
   `c1`, so the orphan check matches exact tag values only) — and the
   Wine process lingers after ` OK: N compiled, 0 failed, M skipped, Tm:Ts`, so the job streams
   `podman exec` into the per-job log
   (`<repo>/target/compile-<map-dashed>/rc-<jobId>.log`, one log per job: L3) through
   `DockerRuntime::exec_in_background` (cs2-mcp-common), watches it at 500 ms — lossy +
   incrementally (M1: one non-UTF-8 Wine byte never blanks the log into a false timeout) — for
   `^[[:space:]]*OK: [0-9]+ compiled`; on it the exec is SIGKILLed (the recipe's exit-137
   teardown) and the container-side cleanup SIGTERMs/SIGKILLs only THIS job's tree (the tag read
   ANCHORED (`^CS2MCP_COMPILE_JOB=`, N6) from `/proc/*/environ`, `vrad3.exe` children included,
   `wineserver`/`services.exe` excluded by comm (a tagged wine bootstrap would be swept, N6); a
   concurrent `model_compile`/
   `particle_compile` is never swept; nothing matched = nothing killed, said in `cleanup`, M3;
   the reported `remaining` ignores zombies and re-polls up to 2 s — the live lesson from a kill
   that said remaining=1 where a scan found nothing),
   and an exec exit re-reads the log once more before a `failed` is written (M2). A process exit
   without the line is `failed`, the deadline (1500 s default, max 3600 s) ends it `timeout`,
   and the transient `crun: … status: No such file or directory` exec failure is retried exactly
   once; the spawned run task is supervised and over-old running jobs are reaped (M4), and
   `ExecHandle::finished` answers `Err` instead of panicking. Background job
   (`HammerState::compile_jobs`, bounded 8, one at a time per server; the check-and-insert is
   one operation on the store's mutex — `CompileJobs::start`, so the write gate is NOT held:
   L7): start returns `{jobId, state:"running", log, vpkCandidates}` at once, `compile_map
   {jobId}` polls (`running|ok|failed|timeout`, elapsed, OK summary, vpk path + `vpkForm` +
   `vpkUpdated`, log tail, failLines, `sourceChanged` (L2: the .vmap's start mtime vs now), the
   `cleanup` report). The output is resolved by the shared `vpk_candidates(app_root, addon,
   content_path)` helper — run_map resolves the same list through it (one lookup, two tools) —
   covering BOTH candidate locations
   for a sub-directory map (`maps/sub/x.vpk`, `maps/x.vpk`; the refreshed one decides, both are
   named when neither advanced: M8, unverified live). The target is `path` (tool-validated like
   `open_map`'s `maps/…vmap` shape) or the active document's map; the compiler reads the SAVED
   file, so the active-document target is refused `-32016 UnsavedChanges` while dirty, dirty-
   unknown or unsaved edits counted (save_map first); `mode:"nav"` adds the recipe's `-nav`
   EXTRA_FLAGS (only nav + map resource, world from the existing vpk) and refuses when no
   candidate vpk exists anywhere (M8); and when the engine holds the target's compiled .vpk
   mounted — `game.local` (through run_map's `read_game_local`: connected first, the live
   `-32024 NoLocalPlayer` answer resolves to inGame:false) or, without it, this server's own last
   OK load of the map (the H1 fallback, review-2 M9) — the start is refused `EngineBusy` with an
   `run_map {unload:true}` hint (a different
   mounted map, or an unreadable `game.local`, is not a refusal: M9; a bare-stem mounted name for
   a sub-directory map refuses only when NO top-level `maps/<stem>.vmap` source exists — the
   ambiguous case notes instead of refusing). **Shutdown and orphans:** EVERY job left without a
   cleanup report when the server goes away (stdin EOF, SIGTERM, SIGINT — a `Running` one, but
   also the closed-without-cleanup `failed` paths, N5; batched into ONE kill script per
   container) is not left behind —
   `main` runs `shutdown_cleanup` (bounded at 5 s, and it FLAGS the store shutting down first, N7:
   a start that races the shutdown is refused instead of orphaned; the signal exit leaves the
   conventional `128+<signal>` status) before it
   exits, and the next start scans the container for `^CS2MCP_COMPILE_JOB=` tags: a tag whose VALUE this
   server knows is ended with the tagged kill, a foreign one is REPORTED and refuses the start
   (`EngineBusy`, `reason:"foreign_compile_job"` — never auto-killed) so two compilers never race on
   one `.vpk`. A `save_map`/`reload_map` of a map whose compile is running adds a L2 note to its
   result (not a refusal). Mutability `Writer` (it
   writes the vpk), no bridge methods; tests `tests/compile_map.rs` (30, per-behavior fake
   podman, incl. the orphan scan/foreign refusal/own kill, the NoLocalPlayer and no-game.local
   memory-fallback refusals, and the shutdown cleanups (running, closed-without-cleanup,
   batched)) + the module's
   unit tests, `tests/stdio.rs` (stdin EOF and SIGTERM over the real binary); live verification is
   the orchestrator's).
3. **Entities**: `create_entities` (`edit.batch` of `entity.create` [+ `entity.setKeyValues`], one
   undo group, `expectDoc`, per-op `applied`/`warnings`, handles stripped, `PartialFailure` with
   `failedIndex`, no rollback), `undo_last` (`undo.undo`, guard on `undoName` prefix `MCP:`).
4. **File reads**: `list_entities`, `find_entities`, `get_entity` done; `list_meshes`/`get_mesh`
   (**mesh file route done, WP-A**) over the
   SAVED `.vmap` with `staleRead` + `source.fromDisk` — the reader decision is (b), and the same
   DMX parse now also extracts the CMapMesh records (`src/vmap/mesh`, ported from
   `../idabins/worknotes/mcpserver/vmap_reader.py` `_mesh_from_element`: position stream ->
   world vertices through `angle_matrix` origin/angles/scales, single-loop edge walks for faces,
   the `faceData` `materialindex` stream for per-face materials; golden-tested against
   `fixtures/vmap/*.meshes.json` in `tests/vmap_mesh.rs`, tool tests in `tests/mesh_file.rs`,
   real-tree `#[ignore]`s on `template_wingman` (18 meshes) and `cs_script_demo` (70, 15
   rotated, with *oracle* world vertices for three of them since review M3 — unit normals alone
   pass for any rotation). `get_mesh` without `path` answers from the session's geometry cache for
   the active document first (`source:"bridge-cache"`, review M2), so its indices are the editing
   tools'; `list_meshes` is the file route only, like the POC. The live/creating mesh tools (WP-B)
   and the server-side face/vertex edits (WP-C) remain.
5. **Renders**: `render_view` (`view.render`; camera modes: live viewport / origin+angles /
   origin+lookAt / orbit; horizontal fov; PNG under `<app>/renders/`, inline image block, blank-image
   note; `NoViewport`) and `render_world` (`world.render`; map running in the engine; 16..8192 px,
   inline <= 1920x1080; `NoGameWorld`). Both read-only; both feed workflow 07.
   **Done (WP-D)**, `src/tools/render.rs`: camera code ported from the POC handlers, the bridge's
   bounds mirrored (16..4096 & 8388608 px view; 16..8192 & 33554432 px world; `(1, 179)` fov;
   `render_name`), client-side before anything is sent. Mock arms in `crates/cs2-mcp-common/src/mock.rs`
   (`hammer_render_reply`, `.renders()` + knobs `view_render_fail`/`world_no_map` (the black
   empty-scene-world image, review-2 L6)/`world_no_scene_world` (the `NoGameWorld` error)/
   `render_dir`);
   contract tests `hammer-mcp/tests/render.rs` (19) + the server-level round trip in
   `tests/server.rs`. Differences from the POC: the default image block is always downscaled above
   1920x1080 (the shared `inline_policy`, like `model_render`/`particle_render`) instead of the
   POC's plain re-compression, and there is no client-side default stem (review-2 L3: an unnamed
   render carries no `name` and the bridge's `render_<tick>_<seq>` names the file). POC-vs-bridge disagreement
   resolved for the bridge: the C dispatch answers `-32003 NoActiveDocument` (the taxonomy's
   `NoDocument`) when Hammer has no active document — the POC tool never checks that — so the mock
   arm carries the gate and the test pins it. Live acceptance (e2e_render.py /
   e2e_world.py ports) is the orchestrator's.
6. **Geometry**: `create_mesh`, `create_block` (box/wedge/cylinder, materials top/bottom/side),
   `set_mesh_geometry`, `transform_mesh`, `move_mesh_vertices`, `extrude_mesh_face`,
   `delete_mesh_faces` (server-side ops from `geometry.py` -> full rebuild via `mesh.setGeometry`),
   `set_mesh_material`, `delete_nodes`. Welding, CCW faces from the visible side, pivot re-centre.
   Port `geometry.py` with its tests first (pure functions), then the tools.
   **Done (WP-B, branch `hammer/mesh-b`; live acceptance is the orchestrator's):**
   `src/geometry.rs` is the full port (`validate`, `material_params`, `face_normal`, `centroid`,
   `bounds`, `orient_outward`, `box`/`wedge`/`cylinder`, `rotate_z`, `transform`, `mirrored`,
   `extrude_face`, `move_vertices`, `delete_faces`, `flip_faces`) with its unit tests, and
   `src/tools/mesh_live.rs` implements `create_mesh` and `create_block` over `mesh.create`,
   `set_mesh_geometry` over `mesh.setGeometry` and `set_mesh_material` over `mesh.setMaterial`
   (all faces) or a `mesh.setGeometry` rebuild (the `faces` argument). Every one of them validates
   locally first (nothing is dispatched for a bad mesh, a zero-area face, an out-of-range
   coordinate, a material the bridge's 260-byte buffer cannot hold, or a request over the bridge's
   1 MiB line buffer), sends `doc.active`'s `expectDoc` + an explicit `deadlineMs`, reports the
   bridge's own `MCP: …` undo label and an `undoNote`, and never saves. The mock serves the three
   methods (`cs2-mcp-common::mock::hammer`, arms + pins in `tests/mock_entities.rs`).
   **Done (WP-C, branch `hammer/mesh-c`; live acceptance is the orchestrator's):** the four POC
   rebuild operators `transform_mesh` (`translate`/`rotateYaw`/`scale`/`pivot`, a mirroring scale
   flips the winding), `move_mesh_vertices`, `extrude_mesh_face` (`newFaces`/`capFace`) and
   `delete_mesh_faces`, in the same `src/tools/mesh_live.rs`, each one: read the mesh's current
   geometry -> `crate::geometry` operator -> [`rebuild`] (the very `send_geometry` path
   `set_mesh_geometry` uses, so the target gate, the request limits, the counter, the undo label and
   the geometry cache behave identically) -> the result reports the POC's `geometrySource`. The
   **geometry source**, in the POC's order: (1) this session's cache of what the server wrote
   (`"bridge-cache"`); (2) else the SAVED `.vmap` via `crate::vmap` `mesh_by_id` (`"file"`),
   **only while the live document is clean** — no unsaved MCP edits and the bridge's `dirty:false`;
   a dirty or dirty-*unknown* document is refused before anything is read or sent (`-32016`
   `UnsavedChanges`, the code `reload_map` uses, with a hint to `save_map` first or to send the
   geometry with `set_mesh_geometry`). This refines the POC, which reads the file anyway and only
   warns that unsaved in-editor edits are replaced. The file route also refuses two meshes it
   cannot place correctly, both `MeshGeometryUnavailable` and retry-safe (nothing sent): one whose
   own object-properties `scales` is not the identity (review-2 M1, `reason:"scaled node"`) and one
   whose file ancestor chain is not proven (review-3 NEW-5, `reason:"unsupported parent"` /
   `"unrooted mesh"`). nodeIDs are one id space: the bridge resolves
   the live node id (`geo_target_mesh` -> `geo_find`) and the file route the `nodeID` attribute a
   save keeps (the POC round-tripped original mesh nodeID 3 through its own file geometry live:
   `idabins/worknotes/evidence/2026-10-07-geometry.txt` item 2). An untitled document is
   `MeshGeometryUnavailable`, an unmapped Windows path `PathNotMapped`, a mesh the file does not
   have `MeshNotFound` (WP-A's wording); every operator validates its arguments *and its result*
   locally (index/face out of range, empty set, zero scale, zero distance, a missing
   translate/rotateYaw/scale, a move that flattens a face, a coordinate the bridge would refuse) so
   a rebuild never reaches the bridge's `geo_reset_mesh`. Tests: `tests/mesh_ops.rs` (36: both
   sources, the dirty/unknown, scaled-node and unproven-parent refusals, the local argument/result
   checks, the review-4 R4-1 sticky-cache scenarios and the review-2 M3 / review-3 NEW-1..3
   predecessors of them) + the `mesh_live.rs` unit tests.
   One gap to close later: the per-face `set_mesh_material` rebuilds from this session's geometry
   cache only (the bridge has no live geometry read — `mesh.get` is `-32011`), so a mesh this server
   did not create or rewrite is refused with `MeshGeometryUnavailable`; wiring that route to the
   SAVED-file source the way the four operators do it changes a shipped tool's contract, so it is a
   follow-up rather than part of WP-C.
   **Review round 1 (`target/orch/review-opus-mesh-1.md`, branch `hammer/mesh-fix1`):**
   - **H1 — the cache is scoped to one document and invalidated where the geometry may have left
     it.** `state.rs` `MeshCache` stores the entries under `doc.active`'s `id` (a node id from map
     A is never served for map B's colliding id, nor after the identity was lost), and the cache is
     dropped at the POC's five places: `note_document` (the `_current_doc` clear; `doc.active` is
     read before any cache lookup, so a tab switch cannot win the race), `undo_last` (ours or not,
     error included), `delete_nodes` (per id on success; the whole cache when the delete failed as
     maybe-executed — review-3 NEW-2), `reload_map` (the id survives a reload — only the
     epoch moves), `open_map`, and any write that failed with an outcome-unknown class
     (`send_geometry`/`set_all_faces_material`; since review-3 NEW-2 that failure clears the whole
     cache through `live_common::mutator_failure`). Test sources: `tests/mesh_live.rs` (the four
     scenarios), `state.rs`'s unit test. The epoch is deliberately *not* the key: the live
     bridge only moves it on a document-identity change (an id change or a reload, both handled),
     while the mock moves it on every mutation, which would drop every entry after this server's own
     write. Tests: `tests/mesh_live.rs` (the four scenarios), `state.rs`'s unit test.
   - **H2 — `create_block` caches the caller's `material`.** `{material:A, sideMaterial:S}` sends
     base S with `[A, A, null…]` (`material_params` picks the most common one), so the cache resolves
     every `null` to **A**, not to the wire base; otherwise the first per-face edit silently
     repaints the faces that use A with S.
   - **M1 — a face-count mismatch is reported, not cached.** The bridge skips `faceMaterials` unless
     Hammer's face count equals the one sent (`geo_build`: `g->nmat && out.size == g->nf`), so on a
     mismatch the result carries `faceMaterialsApplied:false` and a note naming both counts (and
     whether the per-face materials were skipped), and the entry is dropped — the geometry Hammer
     built is not the geometry that was sent.
   - **M2 — `get_mesh` and the editing tools agree on the face indices.** Without `path`,
     `get_mesh` answers from the same cache first (POC `_current_mesh` order,
     `source:"bridge-cache"`, `staleRead:false`, its own note) and falls back to the SAVED file
     (`source:"file"`) only when the server has nothing for the mesh; the per-face `faces` argument
     and the operators' indices are that view's.
   - **M3 — the rotation has an oracle golden.** `src/vmap/mesh`'s unit tests hand-check
     `angle_matrix` (yaw 90: +x->+y, pitch 90: +x->-z, roll 90: +y->+z) and a synthetic
     `angles=[0,90,0] scales=[2,3,1] origin=[10,20,30]` element, which also pins
     scale-then-rotate-then-translate; the real-tree test compares three rotated `cs_script_demo`
     meshes against the Python oracle's world vertices (unit normals alone pass for any rotation).
   - **Lows**: every `-32603` from `mesh.setMaterial` is pre-`undo_open` (`MeshEmpty`, retry-safe, no
     counter bump — `setGeometry` stays conservative: it clears the mesh first); the face/coordinate
     checks run on the rounded values and on their f32 widening (what the bridge reads), with a
     scale-aware minimum area; `get_mesh`'s dirty note only applies to the active document and says
     the edits are not *included*; a missing `material` and unknown arguments are refused (the
     registry does not enforce the schema); a missing index array reads as the empty list like the
     oracle; the shared mutator plumbing lives in `src/tools/live_common.rs` (review L7), so the
     next cross-cutting fix lands once. Nits: the cache stores the rounded vertices; a negative
     bridge-reported node id is "unknown" (no cache, a note); an empty `sideMaterial` falls back to
     the cap's material like the POC's `or`.
    - **Review round 2 (`target/orch/review-opus-mesh-2.md`, branch `hammer/mesh-fix2`):**
      - **M3 (review-2) — the cache was stamped against the document's undo stack.** Each entry
        carried the `topSerial` observed right after this server's own successful write (one extra
        `doc.active` per write; the mesh.* replies carry no undo fields, with the pre-write
        serial + 1 as the fallback when that read failed) plus this server's undo-group count at
        that moment, and `current_mesh` / `set_mesh_material {faces}` / `get_mesh` (no path) served
        an entry only while the read document's `topSerial` still equalled `stamp + (this server's
        groups since the entry)`. **Superseded by review round 4 (R4-1 below)**: the prediction was
        evaluated against whatever group was on top *now*, and an undo can bring an older one back
        at exactly the predicted numbers.
      - **L2 (review-2) — the write rides the source read.** `current_mesh`'s `doc.active` is
        passed through to `send_geometry` (no second read): a tab switch during the `.vmap` parse
        cannot land doc A's file geometry in doc B — the bridge's own `expectDoc` gate refuses it
        pre-write. Mock seam: `set_hammer_doc_switch_after(n)`.
      - **L1 (review-1) — pinned, not changed.** The orchestrator verified live that an `undo_last`
        past the save point makes `ChangedSinceSave` answer `dirty:true` while the unsaved counter
        has saturated at 0; `undo_last_past_the_save_gate_is_the_dirty_flag` pins that the dirty
        gate refuses the operator then (the M3 stamp refuses it too, and H1's undo clear already
        dropped the entry).
      - **M1 (review-2, the scaled file mesh) — conservative refusal.** A file `CMapMesh` with a
        non-identity object-properties `scales` is refused with `MeshGeometryUnavailable
        {reason:"scaled node"}` naming the scales (the file's vertices are already scaled to world
        space but `geo_reset_mesh` never resets a node scale — live behaviour *unverified*, so the
        gate errs safe). New committed fixture `probe_scaled.vmap` = `probe.vmap` with nodeID 3
        patched to `scales [2,2,1]`; the patcher is committed as
        `hammer-mcp/fixtures/vmap/patch_scales.py` and reproduces the fixture byte for byte
        (review-3 NEW-6, `fixtures/vmap/README.md`).
      - **L3 (review-2) — the bridge names unnamed renders.** No `name` argument -> no `name` on
        the wire; the bridge's `render_<GetTickCount64>_<seq>` default is unique per bridge, so
        two concurrent hammer-mcp processes can never collide on a same-second stem.
      - **L4 (review-2) — the path problem only kills an inline render.** `inline:false` answers
        the bridge's own result with `hostPath: null` + a note (and skips the local pixel decode —
        `localStats` is inline-only now); `RenderFileMissing`/`PathNotMapped` are no longer
        `retry_safe` (the render already succeeded, so a retry re-renders for the same path error).
      - **L5 (review-2) — the operators stop costing quadratic comparisons.** `move_vertices`
        walks a `Vec<bool>` pick mask, `delete_faces` keeps/renumbers through a `Vec<usize>` remap
        table, and `index_list_arg` refuses a list longer than the mesh's index space ("at most
        one entry per index") before any geometry math runs.
      - **L6 (review-2) — docs/mock only.** The wording (module header + the `render_world`
        description) says the engine renders its empty scene world with no map: expect a black
        image and the blank note. The mock's `world_no_map` arm models exactly that (black PNG,
        all-zero stats — the live-verified shape) and the new `world_no_scene_world` knob is the
        rare `GetGameSceneWorld == null` case that stays `-32021 NoGameWorld`.
      - Nits: N1 the transform/extrude arguments parse before the source route runs (no
        `UnsavedChanges` for a bad `scale: 0`); N2 the `rotateYaw` schema names its sign (+ = CCW
        seen from above); N3 `zfar` alone is checked against `(0, 1e6]`, and `render_world`'s
        against its default `znear` 4 — a view's effective `znear` is the viewport's, so the module
        claim names that exception; N4 the mock's camera echo matches the real replies (no
        `aspect` on world, view fallbacks 1/9000, world's required-camera checks first) and the
        mock's `error` knob no longer mutates the world behind a pre-write refusal; N5 both
        transform tests pin the literal hand-checked vertices; N6 the unsaved-edits refusal says a
        save made in Hammer's UI is not seen by this server (the counter resets on our `save_map`).
    - **Review round 3 (`target/orch/review-opus-mesh-3.md`, branch `hammer/mesh-fix3`).** The
      reviewer verified in `hammer.dll` that `topSerial` is the serial of the group on top of the
      stack (`CMapUndoGroup.serial` off one process-global counter), that `Undo` pops the group and
      never rewinds that counter, and that every bridge write opens exactly one group, a write that
      fails *after* `undo_open` keeping its group. The live serial probe
      (`target/orch/serial-probe.log`, raw bridge m2.16) adds the fact that matters most for round
      4: **`topSerial` goes down on an undo** and the undo *consumes* a serial
      (create, create, undo, create reads `1, 2, 1, 4` — `3` is never seen).
      - **NEW-1 (High) — the cache stamp carried `undoDepth` too.** A user Ctrl+Z followed by *any*
        MCP write (the review's scenario: write node 7, user undo, keyvalue write) left the serial at
        `stamp + 1` while the popped group left the depth one lower, so entries recorded
        (`topSerial`, `undoDepth`) and were served only while **both** replayed through this
        server's own undo-group count. **Superseded by round 4's sticky rule** (the depth half did
        catch that scenario, but the whole per-entry prediction was still satisfiable by an older
        group; the tests keep the scenario). Mock seams: `hammer_user_undo`, `hammer_user_redo` and
        the counters `hammer_user_undos`/`hammer_user_redos`.
      - **NEW-2 (Medium) — a maybe-executed failure clears the whole cache.** `mutator_failure` (and
        the `edit.batch` / `node.delete` maybe branches) now drop every entry, not just the node the
        failed write named: an over-counted group plus one uncounted UI group re-aligns serial *and*
        depth with the stale prediction. Test
        `a_maybe_executed_failure_clears_the_whole_mesh_cache` (the over-count is produced with
        `-32099` → `Kind::UnknownWire`, because the mock's `-32603`/`-32700`/`-32600` knobs
        deliberately carry the op's side effect, review-2 N4); negative run serves the stale entry.
      - **NEW-3 (Low) — cache only on this write's own one-group step.** The post-write `doc.active`
        was trusted only when it showed `pre + 1` on both fields; anything else (a concurrent group
        absorbed into the stamp) cached nothing and added the "undo stack did not advance by exactly
        this write's one undo group" note, with the `pre + 1` fallback for a failed post-write read.
        **Kept in spirit by round 4** (the note text survives, the fallback does not: an unverified
        post state now caches nothing). Mock seam `set_hammer_group_after_write(n)`. Test
        `a_concurrent_group_after_the_write_keeps_the_geometry_out_of_the_cache`.
      - **NEW-4 (Info) — documented, still owed a live check.** A Hammer UI edit that records into the
        *current* top group instead of opening its own moves neither serial nor depth, so no stamp —
        and no round-4 state comparison — can see it (`state.rs` `MeshCache`, next to the R4-1 rule).
        Live check for the orchestrator: MCP write → drag that mesh in the UI → `doc.active` should
        show `topSerial + 1`.
      - **NEW-5 (Low) — the file route refuses an unproven parent chain.** `crate::vmap::mesh` now
        builds a `children`-edge parent map (`children_parents`, first-owner-wins like the oracle's
        `_link_parents` tree edges) and records each mesh's ancestor classes; the file route accepts a
        mesh only under `CMapGroup`/`CMapEntity` below `CMapWorld` (`MeshRecord::parent_problem`,
        `reason:"unsupported parent"`/`"unrooted mesh"`). Evidence, file-verified not live-verified:
        in the whole installed content tree (23 maps, 4059 meshes) every mesh's chain is exactly that,
        the children's stored origins track their container's world origin rather than `(0,0,0)`, and
        a yaw-180 brush `CMapEntity` in `toolscene_default.vmap` has a child storing its very origin —
        so a container's own transform is a pivot, not a transform on the stored geometry, and a
        value-based refusal would have refused ~86% of real meshes on a contradicted premise. Unit
        test `the_parent_gate_accepts_proven_containers_only` (deformer/instance/prefab/orphan/cycle
        cases), `#[ignore]`d corpus test `real_tree_corpus_has_no_unproven_parent` ("4059 meshes in 23
        maps, none refused").
      - **NEW-6 (Nit) — the scaled fixture is reproducible from the repo.** `patch_scales.py` (the
        binary-DMX attribute patcher) moved from the worker's uncommitted scratch into
        `hammer-mcp/fixtures/vmap/`, gained a no-argument mode that regenerates `probe_scaled.vmap`
        from `probe.vmap` and verifies it **byte for byte** (`--check` = verify only), and is
        documented in `fixtures/vmap/README.md` together with the new fixture table row (nodeID 3's
        `scales` `[1,1,1] -> [2,2,1]`, offset 115560, sha256 `a6e6da2f…`).
      - **NEW-7 (Nit) — the `inline:false` soft note.** The "give the call a `name` and fetch it
        through a mapped path" clause is gone (a name does not affect mapping); the `--path-map`
        advice stays.
    - **Review round 4 (`target/orch/review-opus-mesh-4.md`, branch `hammer/mesh-fix4`).** The
      live serial probe (`target/orch/serial-probe.log`) fixes the model the cache hangs on:
      `topSerial` is the **top group's** serial (it goes *down* on an undo), the counter behind it
      is never rewound, and an undo **consumes** a serial (`1, 2, 1, 4` — `3` is never seen).
      - **R4-1 (High) — the per-entry serial+depth prediction is replaced by a sticky whole-cache
        rule.** The prediction was evaluated against whatever group was on top *now*, so an undo can
        expose an older Hammer-UI group whose numbers are exactly the predicted ones:
        `set_mesh_geometry` (s, d) cached → user drag X (s+1, d+1) → any counted MCP write G (s+2,
        d+2) → user Ctrl+Z undoing G → X on top at (s+1, d+1) = the prediction → the operator served
        the pre-drag geometry and silently reverted the drag. Now `MeshCache` holds one `expected`
        `(id, topSerial, undoDepth)` — the state right after this server's last successful,
        cache-eligible write — and **every** `doc.active` read (`live_common::read_active_document`,
        the one helper all of them go through, `session_status` and the file routes included) compares
        the observed state with it before anybody looks at the reply: any difference, or an
        unreadable reply, drops **every** entry and makes the observed state the new `expected`. A
        write re-establishes it only when its post-read shows exactly its own one undo group
        (`topSerial` strictly up — serials skip — and `undoDepth == pre + 1`), which also keeps the
        batch case: the other entries stay valid because the whole document moved by exactly one
        group this server made. **m2.17: that post state is the write's own reply** (`doc`, read
        inside the write's dispatch — see the m2.17 bullet below), so the "post-read" is no longer a
        second request. A counted write whose post state is not read (`edit.batch`,
        `entity.setKeyValues`, `node.delete`) drops the whole cache through
        `live_common::note_unverified_edit`. Tests: `a_user_undo_of_an_mcp_write_does_not_expose_an_older_ui_group`
        (both the keyvalue and the mesh-write spelling of step 3),
        `an_mcp_write_after_a_user_undo_does_not_revive_the_mesh_cache`,
        `a_user_edit_then_a_user_undo_between_two_calls_keeps_the_cache` (net no change: the state
        is the entry's own, a hit, and the document really holds that geometry), the state.rs
        sticky-rule unit tests. Negative runs: with the comparison removed the first two serve
        `geometrySource: "bridge-cache"` again; on the pre-fix code (f5d7804) the R4-1 scenario
        serves the stale entry.
      - **R4-2 (Low) — the note no longer blames the wrong thing, and the first write after an undo
        is cached.** The note is now "the document's undo stack did not advance by exactly this
        write's one undo group (a Hammer UI edit, an undo or another client in between?)" and the
        acceptance predicate is `undoDepth == pre + 1 && topSerial > pre.serial`, so
        `undo_last` → write → operator works without a `save_map` (the write after an undo *is*
        cached: serials skip, depth + 1). Test `a_write_after_an_undo_is_cached_again`; negative run
        (the old `pre + 1` both-halves predicate) answers `UnsavedChanges` instead.
      - **R4-3 (Low) — the mock's serial model matches live, and the redo test flipped.** `op_undo`
        and `user_undo` now consume a serial (`next_serial += 1`), and `user_redo` re-pushes the
        group's state as a **new** group at the next serial (what Hammer really does is *unverified*
        — m2.17 implements the bridge method `undo.redo` but no server tool calls it, so the mock
        still does not serve it — and this is the conservative reading the committed
        `bump = 1` serial model implies). So a redo moves the state and the sticky rule misses:
        `a_user_undo_then_redo_misses_the_mesh_cache` replaces the old "keeps the cache valid" test
        (which pinned a hit under the old mock + old prediction). Live check still owed: create,
        create, undo, redo, `doc.active` (is the redo record's serial new, e.g. `3/2`?) — and now
        `undo.redo` itself (m2.17's live probe 4 in `bridge/protocol-diff-m2.17.md`), which the
        orchestrator runs raw on the bridge, not through this server.
      - **m2.17 (WP-B) — the write's own reply carries the state the cache is validated against.**
        Every undoable write (`mesh.create`/`setGeometry`/`setMaterial`, `node.delete`,
        `entity.create`, `entity.setKeyValues`, `edit.batch`, `undo.undo`) now answers
        `doc: {id, topSerial, undoDepth}` — the same three numbers `doc.active` reports, read on the
        bridge's main thread **inside the write's dispatch** (`bridge/protocol-diff-m2.17.md`).
        `live_common::post_write_document` prefers it and only falls back to the post-write
        `doc.active` read when the reply has no `doc` (a bridge older than m2.17; mock knob
        `set_hammer_write_doc(false)`), so a mesh write makes **one** `doc.active` read now (its
        pre-write `expectDoc` read) and no document read can land between the write and the state
        the entry is validated against. That is what closes review-5 L2 (two UI actions inside the
        old write/post-read window): the pair is now caught by the *next* read, not attributed to
        the write. `MeshCache`'s sticky rule, `settle`'s predicate and the "not remembered" note are
        unchanged. The two tools whose result is echoed verbatim (`delete_nodes`,
        `set_entity_keyvalues`) **strip** the `doc` member (their cache policy does not consume it);
        the mesh tools build their output field by field and never surface it. Tests:
        `a_write_without_the_reply_doc_falls_back_to_its_own_read` (one read with the m2.17 reply,
        two with the older bridge, the entry committed either way), the `doc.active`-count
        assertions in `transform_mesh_reads_a_foreign_mesh_from_the_saved_vmap` /
        `move_mesh_vertices_rebuilds_a_session_mesh_from_the_cache` (1, not 2),
        `live_common::tests::reply_doc_state_is_the_writes_own_tail`,
        `crates/cs2-mcp-common/tests/mock_entities.rs::undoable_write_replies_carry_the_document_state`
        (every write arm's raw reply, one group per write, plus the no-`doc` older-bridge case).
7. **Forbidden list** test: `console.command`, `undo.redo`, `selection.*`, raw
   `mesh.*`, `material.assign`, `entity.delete`, `control.shutdown` are absent from `tools/list`. (`job.cancel` left the list: `job_cancel` and
   `compile_map {cancel}` end bridge jobs. The `job.*` methods are sent ungated and are not declared in
   ToolSpec, because declaring them would make the registry gate the tools on them.)
8. **Acceptance** (`research/`): port `e2e_render.py`, `e2e_world.py`, `e2e_geometry.py`,
   `e2e_live.py` as scripts that talk MCP over stdio to the Rust binary.

## Definition of done
- Contract tests green on the mock; acceptance scripts pass live on 2000930 (render PNG stats
  non-blank, geometry round-trips through save + parse, undo restores node counts).
- Tool docs (descriptions/schemas) say "SAVED .vmap" vs "live" on every read tool.

Status: the mesh half of §6 landed with WP-B (`hammer/mesh-b`): `geometry.rs` +
`create_mesh`/`create_block`/`set_mesh_geometry`/`set_mesh_material`, mock arms and tests green
locally, and with WP-C (`hammer/mesh-c`): the four rebuild operators with the clean-document
geometry-source rule, `tests/mesh_ops.rs` green. `run_map` landed with WP-R (step 2): the
VToolMessage `command` route, `tests/run_map.rs` + the module's unit tests green locally (15 at
landing, 33 after the review-fix rounds; the first live pass ran it: `run-1/2.log` — ok in 6.6 s,
`game.local` remount + `Spawn Server: <empty>`). The
live acceptance (`research/` scripts, a real save + parse round trip, a real compile + run) has
**not** been run by either WP — it is the orchestrator's step, so treat the live half as
unverified.

## Open questions
- ~~`.vmap` reader in Rust~~ **decided (b)**, 2026-10-14: `src/vmap/dmx.rs` ports `vmap_dmx.py`,
  golden-tested against `fixtures/vmap/` (Python oracle output). Entities and (since the mesh
  file-route WP) the CMapMesh records (`src/vmap/mesh`); the mesh edit/live tools still to add.
  Since 2026-10-17 the decoding itself is `cs2-dmx`; `src/vmap/dmx.rs` keeps the oracle's view and
  rules on top of it (binary 9 only, no trailing bytes, `""`-class stubs).
- `edit.batch` ignores client `undoLabel` in m2.8 (always `MCP: batch`): keep the argument, document
  that it is dropped, until the bridge honours it.
- Keyvalue undo does not revert values (`../idabins/worknotes/evidence/2026-10-06-m4-undo.txt`) —
  surfaced: `undo_last` notes it, `set_entity_keyvalues` returns `previous` (m2.15). A real fix needs
  the CDataModel undo bracket (`hammer-undo.md` §C, slots 69/70), untested.
- `entity.get`/`entity.find` are implemented in m2.15 (`bridge/protocol-diff-m2.15.md`, live-verified);
  `doc.list`/`mesh.get` remain bridge work. Live I/O connection edits are not implemented (file
  route `get_entity` reads `connections`).
- **Mesh geometry has no live read** (`mesh.get` is `-32011`, above). The POC's per-face
  `set_mesh_material` therefore needs the geometry this server wrote (`_mesh_cache`); WP-B ports that
  as `HammerState::mesh_cache` and refuses a foreign mesh with `MeshGeometryUnavailable` instead of
  guessing. WP-C's four rebuild operators take the other route the POC has: the SAVED `.vmap`
  (`crate::vmap` `mesh_by_id`, WP-A) *while the live document is clean* — a dirty/unknown document is
  refused rather than rebuilt from a stale file (the POC reads it anyway and warns). Feeding WP-B's
  per-face `set_mesh_material` from the same file source is the remaining follow-up. Open question:
  whether the bridge should grow a real `mesh.get` instead (then no source would need a clean
  document, and the cache could go).
- **POC vs recipe (`run_map`):** the POC's `h_run_map` is an unimplementable-by-design port — it
  calls `map.run` (a **`-32011`** method) and answers `{jobId, state:"running"}` for `job.status`
  to poll (also `-32011`). WP-R follows the proven recipe instead: the VToolMessage `command`
  channel (`map <name> nomapvalidation=1`), with the engine's own console lines as the verdict and
  no job id at all. The tool therefore has **no** `map.run`/`job.status` capability to gate on, and
  its result is a synchronous-ish `state: ok|failed|timeout|unknown|unconfirmed` (the engine load
  itself stays asynchronous). The POC's `job_grace_s` survives as the `note_external_job` window
  (`EngineBusy`, capped at 120 s; the load is *not* a bridge job, so the grace is trusted until it
  expires instead of being cleared by `control.status` — review-1 L1 — and it is released as soon as
  the load settles). Review-1 also closed, in the same module: the stale-mounted-vpk case (`remount`:
  `disconnect` + the idle level before `map`, H1), the unanchored verdict (M5: the success/failure
  lines only count after this call's own `Loading map "<name>"`), the duplicate post (M6: no retry
  once a post may have been delivered; `delivered:false` → `unknown`; a second call settles the
  previous unresolved load or refuses `EngineBusy`), the shared payload file (M7:
  `HammerState::vtm_gate`), the sub-directory vpk fallback (M8: only when it is the only candidate
  and newer than the `.vmap`; `vpk.form`, `data.flattenedRejected`), the `;`-joined mode line (L4:
  one post each, as the recipe does), the recreated console.log (L5: inode change restarts the read,
  and a missing one says `-condebug` is required) and the read-back deadline (L6: the remaining
  budget, not the call's).
- **POC vs bridge:** `geometry.py` and the bridge's `geom_parse`/`geo_build` agree on the bounds
  (3..65536 vertices/faces, |coord| < 1e6, >= 3 indices per face, <= 64 materials), so WP-B follows
  both; the one place the POC is looser is the request size, where the bridge's 1 MiB line buffer
  (`MAX_LINE`) closes the connection on overflow — the tool caps the payload locally instead
  (512 KiB).

## Evidence
- 24 tools implemented and tested in the POC (`server.py`; 104 mock tests; `e2e_*` live) [proven].
- Bridge rows `[V runtime]`: `view.render`, `world.render`, `mesh.*` m2.5, `node.*`, `edit.batch`,
  `undo.undo`, `map.save`, `map.reload` (`bridge-protocol.md` §7).
- Mesh writes (WP-B, 2026-10-15): the ported `geometry.py` (unit tests in `src/geometry.rs`), the
  four tools (`src/tools/mesh_live.rs`), the mock arms + pins (`crates/cs2-mcp-common/src/mock/hammer.rs`,
  `tests/mock_entities.rs`, `hammer-mcp/tests/mesh_live.rs`) and the bridge's own request/reply shape
  (`bridge/mcp_bridge.c` `op_mesh_*`, `bridge/test_geometry.py` request shapes) [ported, mock-tested;
  live verification pending — the orchestrator runs it].
- Mesh rebuild operators (WP-C, 2026-10-15): `transform_mesh`, `move_mesh_vertices`,
  `extrude_mesh_face`, `delete_mesh_faces` in `src/tools/mesh_live.rs` over the same
  `mesh.setGeometry` path, with the clean-document SAVED-`.vmap` geometry source and the POC's
  `geometrySource`; `tests/mesh_ops.rs` (20) + the module's unit tests [ported, mock-tested; live
  verification pending — the orchestrator runs it]. The node-id claim the file source rests on:
  `idabins/worknotes/evidence/2026-10-07-geometry.txt` item 2 (mesh nodeID 3 round-tripped through
  its own file geometry with identical world positions) [V runtime].
- Run (WP-R, 2026-10-15): `src/tools/run_map.rs` over the VToolMessage `command` channel
  (`crates/cs2-mcp-common/src/runtime/docker.rs` `vtm_command_payload`/`vtmsend_program_argv`, the same
  helpers `runtime.sh`'s channel probe and bridge load are built on) and the console watch;
  `tests/run_map.rs` (33) + the module's unit tests, and `cs2-mcp-common`'s `vtm_payloads` tests
  [ported, mock-tested; **live verification pending — the orchestrator runs it**]. The live facts it
  rests on: `map <name> nomapvalidation=1` loads an addon map and prints `Loading map "<name>"` …
  `OnSwitchLoopModeFinished( game : success )` 9 s later; a plain `map <name>` fails "invalid map
  name"; `maps` crashes the engine; the tools session is reached with `podman exec … vtmsend.exe`
  and the bridge was loaded in that session (`HANDOFF.md` "Playtest", `playtest/load_map.py`) [V
  runtime]. The VToolMessage payload shape is `vcmd.sh`/`pt_lib.py` `vcmd` [V runtime]. **Open:**
  the engine has not been asked to load a map *by this Rust tool* yet, and which failure wordings
  the engine prints (beyond `invalid map name`) is unverified — an unrecognised failure ends as a
  `timeout` with the console tail. **Review-fix round 2 (this pass):** `unload:true` (the
  `disconnect` primitive, the no-op on `game.local inGame:false`, and the load memory it clears), the
  default watch that fits the tool budget silently (only an explicit cut is reported), the shared
  `vpk_candidates` lookup for the vpk (run_map's stricter flattened rule on top), the lazy-connect fix
  in `read_game_local`, and the L2 note `save_map`/`reload_map` carry while a compile of that map
  runs [mock-tested; the unload idle-level signal is **the orchestrator's** to verify live].
- Compile-map shutdown/orphans (this pass): `hammer-mcp/src/main.rs` (`serve_stdio` vs
  `termination_signal`, then `hammer_mcp::shutdown_cleanup`), `compile_map.rs`'s
  `tagged_scan_script`/`parse_tagged_scan`/`classify_tagged_jobs`/`shutdown_cleanup`, and the
  `tests/stdio.rs` end-to-end pair (stdin EOF + SIGTERM over the real binary) [offline: podman is a
  fake script; the in-container `/proc` walk's permission behaviour is the live fact the orchestrator
  saw (`target/orch/live/compile-2.err`)].
- Bridge m2.17 write-reply `doc` state (WP-B, 2026-10-15): the bridge's
  `sb_add_doc_object`/`sb_add_doc_tail` (`bridge/mcp_bridge.c`) with the pin `HFN_GET_REDO_HISTORY`
  from `sigs/signatures/hammer.json`, the Rust consumer `live_common::post_write_document` and the
  mock's `with_doc` (`bridge/protocol-diff-m2.17.md` has the call chain, the redo evidence and the
  live probe list) [I: offline — `bridge/build.sh`,
  `python3 -I bridge/test_entities.py --static` (`static_write_doc_tail`, `static_undo_redo`) and
  `cargo test -p hammer-mcp -p cs2-mcp-common`; the live promotion is the orchestrator's].
