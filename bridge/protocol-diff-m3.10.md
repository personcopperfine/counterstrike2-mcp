# Protocol changes in m3.10 — lane EDIT (`edit.batch`, `entity.tieMeshes`, `entity.untie`)

Bridge build `m3.10` (Rust bridge) = m3.9 + this group. With it every row the C implements has a
body (M4 complete).

**Three rows moved from "not ported" to served**: `edit.batch`, `entity.tieMeshes`,
`entity.untie`. The method table is unchanged (84 registered / 69 implemented), no new error code,
no changed field. Every other row's state is untouched (the wave-3 lanes leave, among others,
`input.set` unserved).

## The wire, row by row

| Row | m2.20 (C bridge) | this build |
|---|---|---|
| `edit.batch` arm (`:5855-6005`) | `active_doc` → `doc_editable` → `expect_doc_ok`, then the `ops` member (`-32602 "invalid params: ops"`, bare), ONE `undo_open(batch_undo_label(ops))`, the loop, the reply | identical order and strings (`ops::edit::edit_batch_with`) |
| `batch_undo_label` (`:2468-2471`) | `batch_has_create(ops) ? "MCP: batch" : "MCP: set keyvalues"` | identical, as a `&'static CStr` (Hammer keeps the pointer) |
| `batch_subst_handle` (`:2474-2504`) | `"handle":"$N"` → handle N's token, rewritten in place in the 8192-byte op params; the 1536-byte tmp guard uses `slen(q)` | identical, including the guard and the 16-byte handle rows |
| per-op dispatch (`:5917-5951`) | `entity.create` → `op_entity_create` (`:2167-2171`, opens NO group); `entity.setKeyValues` → the recorded-decision + `op_set_keyvalues` (`:2447-2452`, NO group); anything else → the inner `-32011 NotImplemented` row with `data.reason` = the (63-byte-capped) method | identical (`ops::hammer::op_entity_create_no_group` / `op_set_keyvalues_no_group`, `batch_record_decision`, `handle_lookup`/`node_id_of`, `set_keyvalues_target` + `apply_keyvalues`) |
| batch bookkeeping (`:5953-5983`) | the first failure ends the run at that index; `ob.ovf` ORs into `batch_ovf`; the create's handle and `nodeID` feed the 16-row handle table and the 32-entry `recorded` list; `idx` advances only on success | identical |
| batch replies (`:5986-6005`) | `-32012 PartialFailure` with `{"failedIndex":null,"reason":"batch result too large; ... do not retry"}` on a 256 KB overflow; `-32012` with `{"results":[...],"failedIndex":N}` on a failed op; else `{"results":[...],"failedIndex":null,"doc":{...}}` | byte-identical builders (`ops::edit::tests`) |
| `entity.tieMeshes` (`:4113-4431`) | exactly one of `class`/`entityID`; the class string (≤63 B); `keyValues` only with class and only an object; `allowRetie` exactly true/false; the `keyValues` dry pass; 1..64 distinct `nodeIDs`; the target entity's solid/locked/prefab gates; each mesh's found/mesh/prefab/locked/already-tied/retie-locked gates; the first mesh's transform; the all-alreadyTied no-op (no group); then `undo_open("MCP: tie meshes to entity")`, the create-or-reuse, the defensive solid-class check, `apply_keyvalues` (`prev = 0`), `RecordNodesAndParents` BEFORE the `AddChild`s, the emptied-old-entity delete, `DocSetModified` | identical order, labels, caps and every `reason` string (`ops::edit::op_entity_tie_meshes`) |
| `tie_reply_body` (`:4432-4482`) | `{"nodeID"?,"handle","class":"entity","classname","created","origin","meshes":[{nodeID,origin}],"alreadyTied","removedEmptyEntities","keyValues"?,"noop","undoLabel"}` | byte-identical builder; `nodeID` omitted when unreadable, a mesh id `null` |
| `tie_err_node` / `tie_err_undo_group` (`:3998-4028`) | `{"reason","nodeID","parentNodeID"?}`; `{"reason","undoGroupOpened":true,"undoLabel"}` (no `maybeExecuted`) | identical |
| `entity.untie` (`:4485-4564`) | the `nodeID` gates, `geo_entity_node`, solid/locked/non-empty gates, the child-list guards, the children collected BEFORE the move (a child whose `nodeID` is negative skipped, `:4533`), the parent guard, `DocMoveToWorld` (its own group), `DocSetModified`, the reply | identical (`ops::edit::op_entity_untie`) |
| untie reply (`:4551-4559`) | `{"nodeID","meshes":[ids],"entityDeleted":true,"meshesTruncated"?,"undoLabel":"Untie from entity"}` | byte-identical builder |
| error codes | `-32003`, `-32004`, `-32006 UnknownClass`, `-32011` (inner batch row only), `-32012`, `-32018`, `-32022`, `-32602`, `-32603`, `-32603 "response too large (use limit)"` | identical, with the C's 512-byte `data` objects |

## Review round 2: two divergences that were found live, and are fixed here

Both were behaviour differences from the C, not wire shapes; the table above describes the port
**after** these fixes (the earlier revision's "identical" claim in the per-op dispatch row was
wrong).

1. **`edit.batch` opened one undo group per create.** The batch dispatch called the *arm*-shaped
   body (`undo_open("MCP: create entity")`), so a 3-create batch left undo depth 3 with three
   "MCP: create entity" groups instead of the C's single "MCP: batch" (`:5878`; the C's
   `op_entity_create` at `:2167-2171` opens nothing). Found live under Wine (`undoDepth`/`undoName`
   in `bridge/test_edit_batch.py`'s `one_group`). Fixed by splitting each body into an arm wrapper
   that opens the group and a `*_no_group` half the batch calls
   (`ops::hammer::{op_entity_create, op_entity_create_no_group, op_set_keyvalues,
   op_set_keyvalues_no_group}`, all over one `*_with(..., group: Option<&'static CStr>)`), and by
   deleting `edit.rs`'s duplicate `batch_set_keyvalues` in favour of the shared no-group body.
   Host tests now assert the fake's group labels: `edit_batch_creates_open_exactly_one_group`
   (3 creates and a create+setKeyValues batch → exactly `["MCP: batch"]`),
   `edit_batch_keyvalue_only_batch_takes_the_keyvalue_label` (→ `["MCP: set keyvalues"]`),
   `the_create_body_opens_no_group_for_edit_batch` / `the_set_keyvalues_body_opens_no_group_for_edit_batch`
   (the no-group halves open none) and the arm tests (→ `["MCP: create entity"]`).
2. **`entity.untie` listed a child whose `nodeID` is negative.** The C skips it
   (`:4532-4533` `if (cid < 0) continue;`); the port only skipped an unreadable id, so a corrupt
   child could appear as `-1` in `meshes`. Fixed with
   `w.id(child).filter(|c| *c >= 0)`; `untie_skips_children_with_a_negative_node_id` covers it.

## What each body does (and where the C is)

* `edit_batch_with` (`:5855-6005`) is the arm's body after the gate: the `ops` member, the one
  group, the C's four scratch buffers (`results[256*1024]`, `opbuf`/`pbuf[8192]`,
  `rbuf[65536]`), the 16-handle table, the 32-entry `recorded` list and the three reply shapes.
* `op_entity_tie_meshes` (`:4113-4431`) is a 1:1 port of the validation ladder, the group, the
  create/reuse, the keyValues pass, the reparent recording and the two in-band failures.
* `tie_kv_prescan` (`:4066-4107`), `tie_ancestors_ok` (`:4044-4063`), `tie_reply_body`
  (`:4432-4482`), `tie_err_node`/`tie_err_undo_group` (`:3998-4028`) are separate functions, all
  pure except the reply body's reads.
* `op_entity_untie` (`:4485-4564`): the C's `CMapDoc::MoveToWorld` recipe, including the rule
  that the untie opens NO bridge group and the children are collected before the call.
* The two arms are the C's `geo_dispatch` head (`:5612-5659`) and tail, assembled from
  `ops::hammer::write_gate` + `write_finish` (the same helpers the HAMMER-W1 writes use), the
  512 KB `OP_RESULT_CAP` buffer and the `"response too large (use limit)"` text.

## Places the port deliberately differs from the C (no byte on the wire)

1. **The game access is one `Edit` trait (edit.rs), composed the way the mesh lane composes
   `Mesh`.** The tie/untie bodies need reads and calls the Hammer lane's `Write` does not carry:
   `node_parent`, `gd_type`, `node_child_count`, the `AddChild` slot, `RecordNodesAndParents`,
   `DocMoveToWorld` and the classname read. Those sit behind `ops::edit::Edit` (`Nodes` +
   the six new methods), implemented for the live `hammer::win::LiveHammer` in edit.rs and by a
   recording fake for the host tests. The Windows impl binds each method to the generated slot or
   the pinned `HFN_*` entry the C calls (the `AddChild` entry is read through
   `CMapNode_slots::AddChild` + a local `-> u8` signature because the generated typed field is a
   Rust `bool`, the wave-2 lesson).
2. **The batch's statics are heap buffers.** `results`/`opbuf`/`pbuf`/`rbuf`, the tie's `kvbuf`
   and its id arrays are locals/`Vec`s per call instead of C `static`s; the caps are the C's, so
   every overflow point is identical, and the bodies run on the tools main thread only (the
   tick's drain) like the C's.
3. **`tie_reply_body` takes the handle.** The C calls `handle_new(ent)` inside the reply body;
   the port issues it just before the call. `handle_new` is bridge-side state (no game access),
   so the token value and the reply bytes are the same; the ID/classname/origin reads still
   happen in the C's order.
4. **The op params cross the `&str` boundary lossily.** The C hands the body raw bytes; the Rust
   ops take `Option<&str>` (as the HAMMER-W1 writes already do), so a params member with invalid
   UTF-8 becomes U+FFFD per maximal invalid subpart - the same replacement `Sb::addjson` emits for
   the echo. A caller sending invalid UTF-8 in a `keyValues` name/value gets that replacement
   written by `SetKeyValue` where the C would store the raw bytes. No valid-UTF-8 request differs.
5. **`batch_subst_handle` refuses an out-of-range `$N`.** The C compiles `n` as `int` and
   overflows it after ~19 digits, then indexes outside its 16-row table; the port accumulates in
   `i64` with the C's two's-complement wrap and returns without substituting for every index the
   table cannot hold. Same answer for every request the shipped C answers sanely.
6. **`tie_err_node`/`tie_err_undo_group` build in `OpErr`'s 512 bytes** (`op_err`'s buffer), the
   C's own sizes.

## Python parity coverage (the orchestrator runs these under Wine)

* `bridge/test_edit_batch.py` — `batch_ok`, `handle_ref` (`$0` resolves to op 0's handle),
  `b_named`, `one_group` (`undoDepth + 1`, `undoName == "MCP: batch"`), `file_a`/`file_b` and the
  saved origins/classes, `partial_code`/`partial_index`/`partial_results` (create then an unknown
  class), `undo_partial`/`undo_partial_name`/`partial_reverted`: covers `edit.batch`'s success
  and partial-failure paths, the `$N` substitution, the one-group rule and the label.
* `bridge/test_triggers.py` — `methods_advertised`, `tie_a`, `tie_a_origin_is_mesh_origin`,
  `tie_a_meshes_echo`, `tie_a_doc_tail`, `tie_b_entity_form`, `retie_refused` /
  `retie_refused_names_nodes` / `retie_refused_doc_unchanged`, `retie_allowed`,
  `emptied_trigger_gone`, `retie_children_three`, `tie_keeps_world_origin`, `noop_already_tied`,
  `noop_already_tied_doc_unchanged`, `refuse_kv_malformed`, `refuse_not_brush_names_class`,
  `refusals_did_not_open_groups`, `refusals_top_serial_unchanged`, `refuse_point_class`,
  `refuse_point_class_group_noop`/`_group_popped`, `untie`, `untie_entity_gone`,
  `untie_origins_unchanged`, `untie_undo_restores`, `untie_undo_children_back`: covers every
  tie/untie path this lane ports.
* `bridge/test_triggers.py --static` already greps the Rust sources: it reads `BRIDGE_BUILD`,
  the registry rows (`NEED_HAMMER`), the served `HANDLERS` names and the two `HFN_*` pins
  (`HFN_DOC_MOVE_TO_WORLD`, `HFN_RECORD_NODES_AND_PARENTS`). A deeper `--static` for this lane
  would also check, in `crates/cs2-bridge/src/ops/edit.rs`: the three `HANDLERS` names; the
  `"MCP: tie meshes to entity"`, `"MCP: batch"`, `"MCP: set keyvalues"` and `"Untie from entity"`
  labels; that `op_entity_untie` contains no `undo_open` call; the tie's `undo_open` position
  after the `nodeIDs`/keyValues gates; the `CMapNode_slots::AddChild` use; and the
  `-32006`/`-32012`/`-32022` reason strings.

## Tests (host, no game)

`crates/cs2-bridge/src/ops/edit.rs` `mod tests` (32 cases): the C-ordered param refusals for
tie/untie, the batch gate order, every reply/error builder byte for byte, the tie's call order
through the recording fake (recording before `AddChild`, `DocSetModified` before the refusal
data), the noop-without-group rule, the capped progress lists, the emptied-old-entity delete, the
ancestor bound, the batch's `$N`/record-decision/overflow/partial shapes. `tests/ops_edit.rs`
holds the `-32017 PrerequisiteFailed` gate (unverified hammer, body never called) and the
advertised-rows check over the real `wiring::BridgeState`.
