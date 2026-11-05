# Proposed protocol changes for bridge m2.17 (the write-reply document state + `undo.redo`)

Additive over m2.16; no major bump. Two changes:

1. **Every undoable write's result carries the document state it left behind** —
   `doc: {id, topSerial, undoDepth}` on `mesh.create`, `mesh.setGeometry`, `mesh.setMaterial`,
   `node.delete`, `entity.create`, `entity.setKeyValues`, `edit.batch` and `undo.undo`. The three
   numbers are the ones `doc.active` reports, read **inside the write's own dispatch**, on the tools
   main thread, right after the op. This is what hammer-mcp's mesh cache validates a write against
   instead of making a second `doc.active` request (review-4 R4-1 step 3): a follow-up request is a
   *later* drain iteration, so a user Ctrl+Z plus a user edit could land between the write and that
   read and make a two-action sequence look like the write's own single undo group (review-5 L2).
   Nothing can land between the write and this read.
2. **`undo.redo {expectDoc}`** — the documented-but-blocked row is implemented: the document's redo
   stack, `GetRedoHistory(doc)`, through vtable slot 2 (the same apply-and-pop `undo.undo` calls on
   the undo stack).

Sources read for this pass: `../../idabins/docs/data/bridge-protocol.md` §7 (the `undo.undo` and
the `undo.redo` candidate rows, §8), `../../idabins/docs/interfaces/CMapDocHistory.md` (the pair
`[root+0x240]`/`[root+0x248]`, the 4-slot vtable, `GetRedoHistory hammer!0x180885BA0`),
`../../idabins/docs/flows/hammer-undo.md` §B (what `CMapDocHistory::Undo` does and what is *not*
proven: no Hammer Redo call site), `bridge/mcp_bridge_pet.inc`'s `pet.undo`/`pet.redo` (the same
shape, live-verified in m2.13), `bridge/mcp_bridge.c`'s write arms and `sigs/signatures/hammer.json`.

**Status.** Everything below is `[I: offline]`: `./bridge/build.sh` (host unit tests, pins, sdkcheck,
the DLL link) is green, `python3 -I bridge/test_entities.py --static` is green (its new
`static_write_doc_tail` and `static_undo_redo` sections are this pass's offline contract), and
`cargo clippy`/`cargo test` for `hammer-mcp` and `cs2-mcp-common` are green. **Live is the
orchestrator's**: the `doc` tail in a running Hammer, and a real Ctrl+Y-equivalent redo, are not
claimed here.

## §7 method table — changed rows

| Method | Status | Bridge m2.17 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `mesh.create`, `mesh.setGeometry`, `mesh.setMaterial`, `node.delete` | unchanged `[V]`, result gains `doc` | unchanged | unchanged | `{... the op's own fields ..., doc:{id,topSerial,undoDepth}}` | unchanged (`geo_dispatch(..., mutating=1)`) | The tail is appended by `geo_dispatch` after a successful mutating op only — the read rows that share it (`node.list`, `entity.get`, `entity.find`) carry none |
| `entity.create` | unchanged `[V]`, result gains `doc` | unchanged | unchanged | `{handle, class, origin, undoLabel, keyValues?, doc:{...}}` | unchanged | — |
| `entity.setKeyValues` | unchanged `[V]`, result gains `doc` | unchanged | unchanged | `{applied, warnings, previous, valueUndo, doc:{...}}` | unchanged | — |
| `edit.batch` | unchanged `[V]`, the **success** result gains `doc` | unchanged | unchanged | `{results:[...], failedIndex:null, doc:{...}}` | unchanged | The `-32012 PartialFailure` **error** reply is unchanged: there is no `result` to extend, and `hammer-mcp` treats `-32012` as maybe-executed (it drops the whole cache) |
| `undo.undo` | unchanged `[V]`, result gains `doc` | unchanged | unchanged | `{undone, name, undoDepth, doc:{...}}` · `{undone:false, name:null, doc:{...}}` | `GetActiveHistory()` slot 2 (`HVS_HISTORY_UNDO`) | The empty-stack branch carries the tail too (the state did not move), so a client can compare it either way |
| `undo.redo` | **implemented `[I: offline]`** (was `impl 0`, blockedOn `"bridge-protocol.md §7 undo.redo (runtime check pending)"`) | gate `NEED_HAMMER`; refusals in order `-32003 NoActiveDocument` -> `-32018 DocumentNotEditable` -> `-32004 DocumentChanged` -> `-32017 PrerequisiteFailed {reason:"bad_redo_history"}` | `{expectDoc:{id,epoch}}` (exactly `undo.undo`'s) | `{redone:true, name:"<the group that was popped>", redoDepth:int, doc:{...}}` · `{redone:false, name:null, doc:{...}}` (the redo stack was empty: a RESULT, never an error) | `CMapDoc::GetRedoHistory(doc)` (`HFN_GET_REDO_HISTORY`) -> that history's vtable slot 2 (`HV_HISTORY_VT` slot 2, `CMapDocHistory::Undo`, `hammer!0x18086E630`) | see below |

### `undo.redo` — the evidence, and what it does not prove

- **What is called.** `GetRedoHistory(doc)` (`HFN_GET_REDO_HISTORY` = `hammer!0x180885BA0` on
  2000930, the address idabins' `CMapDocHistory.md` names) returns the map root's **redo stack**
  `[owner+0x248]` — the pair partner of the undo stack `GetUndoHistory` returns (`[owner+0x240]`) —
  with the same doc guard (`[doc+0x118]`, the scope stack at `doc+0x98`, the root at `doc+0x80`).
  Both stacks are `CMapDocHistory`, so they share the pinned vtable and slot 2 is
  `CMapDocHistory::Undo` — the apply-and-pop the bridge already calls for `undo.undo` (live-verified
  since m2.5).
- **Why slot 2 is direction-agnostic.** `pet.dll`'s own `CPetDoc::Redo` (`PETFN_DOC_REDO`) is
  literally `(*(doc+0x88) + 0x80)` slot 2 — the pair partner's apply-and-pop — and `pet.redo` was
  verified **live** in m2.13 (`bridge/test_pet_live.py` check 14: undo -> 2 arrays, redo -> 3). That
  is the same `CUndoHistory` implementation in another module (same 4-slot vtable shape, same
  `+0x80` partner, same `+0x90` enabled byte, same `0xC0` group / `0x50` record sizes), which is
  what makes "call slot 2 on the redo stack" a redo rather than a second undo. The ModelDoc
  `CUndoHistory` the protocol page cites shows the same shape (`ApplyAndPop`, direction-agnostic).
- **Why the empty case cannot become an undo.** The bridge gates on the redo history's **own
  `count`** (read by `history_top_info`, the helper `doc.active` uses for `undoDepth`/`topSerial`),
  never on `HasEntries()` — whose override falls back to the history at `[this+0x4B0]` (a *related
  map root*'s stack), which is not the stack the call would then apply. With `count == 0` slot 2 is
  never called, so the undocumented `[this+0x88]`-clear re-route inside `CMapDocHistory::Undo`
  (hammer-undo.md §B step 7: it can forward to `[[g_pActiveMapDocHistory]+0x80]` slot 2) cannot turn
  an empty redo stack into a pop of the undo stack through this arm.
- **What is NOT proven.** That *Hammer's* redo re-applies the state the undo reverted. No Hammer
  Redo call site is documented — hammer-undo.md §B step 8 says the command bound to the `"Redo"`
  string was never isolated, and `CMapDocHistory::Undo` has no direct callers (it is reachable only
  through its vtable cell). So the reply reports exactly what the bridge observed: the redo stack
  shrank (`redone`), which group was on top before the call (`name`), and the redo stack's depth
  after it (`redoDepth`). It does **not** claim the document's nodes/keyvalues changed; the
  orchestrator's live run is what promotes the mechanism (see the live probe list).
- **Where the row stands.** The `bump = 1` / "redo is a new group" model `hammer-mcp`'s mock
  documents (R4-3) is **unverified live** and stays as it is: the mock does not serve `undo.redo`
  (no server tool calls it — the POC's `server.py` lists `undo.redo` under "Deliberately never
  exposed"), so this method is **bridge-only**.

## §8 error codes

No new code. Reused: `-32003 NoActiveDocument`, `-32018 DocumentNotEditable`, `-32004
DocumentChanged` (exactly `undo.undo`'s) and `-32017 PrerequisiteFailed` with the new reason
`{reason:"bad_redo_history"}` (the pair partner is null or fails the `HV_HISTORY_VT` identity check:
`GetRedoHistory` is only called once the document passed `doc_editable`, which proves the *undo*
history's class, not the redo one's). Nothing is retired. The new `doc` tail adds no code.

## §5/§6 notes

- Method count: **79 registered / 64 implemented** (m2.16: 79/63; the flip is `undo.redo`) — the
  count `control.capabilities`/`control.version` derive from one table. `control.version.bridgeVersion`
  and every `"build"` field are **`m2.17`**.
- **New pin** (`sigs/signatures/hammer.json`, `bridge/bridge_pins.h` regenerated by
  `bridge/pins.py`): `HFN_GET_REDO_HISTORY` on the existing `GetRedoHistory` entry (`kind=function`,
  sig locator, `expect[2000930] = 8936352 = 0x885BA0`). The entry, its locators and its
  `sigs/fingerprints/hammer-fp.json` key `GetRedoHistory` already existed and were already resolved
  by the DB's own judge for this build — this pass only adds the `bridge` macro, so the pin inherits
  that verification instead of being a new `proposed` entry. `pins.py` emits the RVA macro, the
  24-byte prologue window (`k_pinBytes`, `40 53 48 83 EC 20 80 B9 18 01 00 00 00 48 8B D9 74 2A 48
  81 C1 98 00 00 00` — the same prologue as `GetUndoHistory`, whose tail reads `[+0x240]` instead of
  `[+0x248]`) and nothing else.
- `bridge/s2sdk.h` gains `CMapDocHistory *(*GetRedoHistory)(CMapDoc *doc);` next to `GetUndoHistory`;
  `hammer_bind` adds its `BIND`. `PIN_MODULE_COUNT` stays 12; no new module, no new error code.
- The diff against the base branch introduces **no `0x` literal** in `bridge/mcp_bridge.c`
  (`bridge/test_entities.py --static`, `static_no_new_hex_literals`): every number is a
  `bridge_pins.h` macro.
- Version bump outside the new code: `bridge/test_entities.py`, `test_modeldoc.py`,
  `test_modeldoc_render_live.py`, `test_pet.py` and `test_pet_live.py` assert the bridge build and
  are bumped to `m2.17` (the same set m2.16 bumped). `test_entities.py` keeps reading its own pass's
  `protocol-diff-m2.15.md` for the entity contract.

### The `doc` tail's shape (why these three fields, and why in `result`)

- **Same source as `doc.active`**: `id` is `"d<docSeq>"` off the bridge's own document fingerprint
  (`doc_fingerprint()` runs first, so the id is current even if the last `doc.active` was long ago),
  `topSerial`/`undoDepth` come from the top undo group (`history_top_info`, the helper
  `doc.active` uses). A client can therefore compare a write's tail with any `doc.active` reply
  field by field — which is exactly what `hammer-mcp`'s sticky mesh-cache rule does
  (`cs2-mcp-common`'s mock and the `MeshCache` unit tests pin a tail equal to the `doc_json()` of
  the same moment).
- **In `result`, not in the envelope**: additive for old clients (an extra member of a result object
  that every consumer reads by name), and `hammer-mcp` strips it from the two tools whose result is
  echoed verbatim (`delete_nodes`, `set_entity_keyvalues`), which do not consume it.
- **Absent** for a failed write (no `result` at all), for a read, and on `-32012 PartialFailure`.
  `hammer-mcp` then falls back to its own post-write `doc.active` read — the older bridge's path,
  kept on purpose and covered by `hammer-mcp`'s `set_hammer_write_doc(false)` mock knob and
  `a_write_without_the_reply_doc_falls_back_to_its_own_read`.
- **Never cut**: the tail is appended *before* each arm's result-buffer overflow check
  (`geo_dispatch`, `entity.create`, `entity.setKeyValues`), so the ~60 bytes it adds can only turn a
  result that already filled the buffer into the arm's `-32603` refusal, never into truncated JSON.
  `bridge/test_entities.py --static`'s `static_write_doc_tail` asserts that order.

## Live probe list for the orchestrator (added to the m2.16 run)

1. `control.version` -> `bridgeVersion == "m2.17"`; `control.capabilities` now advertises
   `undo.redo` (it was absent/`-32011`) and `bridge/test_entities.py` (live mode) passes with its
   `BUILD` bumped to `m2.17`.
2. `doc` tail on a raw write: `mesh.create`/`entity.create` -> the reply's `result.doc` has
   `id == doc.active.id`, `topSerial == doc.active.topSerial`, `undoDepth == doc.active.undoDepth`
   **of that moment** (read `doc.active` immediately before *and* after: the write's tail must equal
   the *after* state, and move by exactly one group from the before state: depth + 1, a strictly
   higher serial). Repeat for `edit.batch` (one group for a 2-op batch), `node.delete` and
   `entity.setKeyValues`.
3. `undo.undo` -> the tail is the **post-pop** state (depth one lower; `topSerial` goes down to the
   new top group's serial, `serial-probe.log`). On an empty stack: `{undone:false,name:null,doc}`
   with the unchanged state.
4. **`undo.redo` (the row this pass promotes):**
   1. with at least two bridge-made groups (`MCP: ...`) on the stack: `undo.undo` (note `name` and
      the tail) then `undo.redo {expectDoc}` -> `{redone:true, name:"MCP: ...", redoDepth}`, where
      `redoDepth` is one *lower* than before the call, and the tail shows the undo stack back up by
      one group at a **new** serial. **The live fact to record**: does the document show the
      undone edit again (the geometry/keyvalues back) — that is what "a Hammer redo re-applies"
      needs and what the offline pass cannot prove.
   2. `undo.redo` again with an empty redo stack -> `{redone:false,name:null,doc}` and **nothing
      changed** — in particular the *undo* stack must not have been popped (the reason the arm
      gates on the redo stack's own count).
   3. after a fresh bridge write (a new group invalidates the redo side, `CMapDocHistory.md`:
      "opening a group in this history destroys `[+0x80]`'s entries"): `undo.redo` ->
      `{redone:false}`.
   4. a stale `expectDoc` -> `-32004 DocumentChanged {reason:...}`, nothing popped; on a
      non-editable document -> `-32018`; with no document -> `-32003`.
   5. `undo.redo` on a document whose `hammer-mcp` counter is in play: `hammer-mcp` has **no tool**
      for it (POC parity), so the probe is a raw `undo.redo` on the discovery file's port.
5. The L2 point the tail exists for, at the tool level: a mesh write whose reply is followed
   immediately by a user Ctrl+Z of that write plus a user edit (two UI actions) must now be
   *rejected* by the next tool call's read (depth/serial differ from `expected`) — the reply's tail
   already fixed the write's own state, so the sweep-up happens on the next read instead of being
   attributed to the write.

## Offline proof

`./bridge/build.sh` is green: `sigcheck.py validate` (0 problems), `sigcheck.py resolve --strict`,
`pins.py` (the committed `bridge_pins.h` matches the regenerated one, 290 macros), `gen.py --check`
(both ABIs), `sdkcheck.py` (16 PASS / 6 SKIP / 0 FAIL), the four host unit tests and the DLL link.
`python3 -I bridge/test_entities.py --static` is **PASS** with the two new sections
(`static_write_doc_tail`, `static_undo_redo`), `bridge/test_modeldoc.py --static` is PASS.
`cargo clippy -p cs2-mcp-common -p hammer-mcp --all-targets` is clean; `cargo test` is green for
both crates (`cs2-mcp-common` 260 passed / 0 failed, `hammer-mcp` 530 passed / 0 failed; new:
`cs2-mcp-common::mock_entities::undoable_write_replies_carry_the_document_state`,
`hammer-mcp::live_common::tests::reply_doc_state_is_the_writes_own_tail`,
`hammer-mcp::mesh_ops::a_write_without_the_reply_doc_falls_back_to_its_own_read`).

**Negative evidence** — one revert at a time, the new tests unchanged:

- dropping the `doc` tail from the mock's write replies (`with_doc` returning `v`): the two
  `doc.active`-count assertions in `hammer-mcp/tests/mesh_ops.rs` (`transform_mesh_..._from_the_saved_vmap`,
  `move_mesh_vertices_rebuilds_a_session_mesh_from_the_cache`: one read per mesh write) and
  `undoable_write_replies_carry_the_document_state` fail — the reply's state is what those tests
  count on;
- `post_write_document` without the reply branch (always the fallback read):
  `a_write_without_the_reply_doc_falls_back_to_its_own_read`'s first half fails (two reads where
  m2.17 makes one);
- the `undo.redo` row back to `impl 0`: `bridge/test_entities.py --static`'s `static_undo_redo`
  fails (the arm, the pin, the prototype and the class gate are all asserted by name).

## Applying this in idabins (not done here)

`../../idabins/docs/data/bridge-protocol.md` §7 needs: the `doc` member added to the eight result
shapes above, the `undo.redo` row moved from "blocked (runtime check) / not implemented" to
implemented with the call chain and the `[I: offline]` marker (promoted to `[V runtime]` by the
orchestrator's live run), the method count 79/64, and §8's `-32017 {reason:"bad_redo_history"}`.
`CMapDocHistory.md`'s "GetRedoHistory -> slot 2" candidate paragraph and `hammer-undo.md`'s §B step
7/8 open question can point at this file. The docs live in the `idabins` checkout, which this pass
must not touch (`AGENTS.md`: work in the assigned worktree).
