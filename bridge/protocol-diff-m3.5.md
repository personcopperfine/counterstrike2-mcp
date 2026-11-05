# Protocol changes in m3.5 — lane HAMMER-W1 (the Hammer document writes)

Bridge build `m3.5` (Rust bridge) = m3.4 + this group.

**Eight rows moved from "not ported" to served**: `entity.create`, `undo.undo`, `undo.redo`,
`entity.setKeyValues`, `node.delete`, `node.setTransform`, `entity.addConnections`,
`entity.removeConnections`. The method table is unchanged (84 registered / 69 implemented), no
new error code, no changed field. `edit.batch`, `entity.tieMeshes` / `entity.untie` and `mesh.*`
stay `-32011 NotImplemented` (later lanes).

## The wire, row by row

| Row | m2.20 (C bridge) | this build |
|---|---|---|
| `geo_dispatch` write head (`:5615-5645`) | `active_doc` → `doc_editable` → `expect_doc_ok`, in that order, then the op, then the `doc` tail, then the overflow check | identical (`ops::hammer::write_gate`, `write_finish`) |
| `entity.create` (`:5705-5753`) | own head, 16384-byte result buffer, `-32603 "response too large (use fewer keyvalues)"` | identical |
| `entity.setKeyValues` (`:5813-5854`) | own head, 65536-byte result buffer, same overflow text | identical |
| `undo.undo` / `undo.redo` (`:5747-5812`) | own head, reply built straight into the envelope, `bad_redo_history` gate for redo, empty-stack result `{"undone|redone":false,"name":null,"doc":…}` | identical (`ops::hammer::undo_step`, `undo_reply`) |
| `node.delete` / `node.setTransform` / `entity.addConnections` / `entity.removeConnections` | `geo_dispatch` with `mutating = 1`, 512 KB buffer, `-32603 "response too large (use limit)"` | identical |
| result fields | `{"handle","nodeID"?,"class","origin","scales","undoLabel"(,"keyValues")}`, `{"applied","warnings"(,"previous","previousTruncated","valueUndo")}`, `{"nodeID","added","count","landed","undoLabel"}`, `{"nodeID","removed","count","remaining","undoLabel"}`, `{"deleted","undoLabel"}`, `{"nodeID","class","origin","angles","scales","undoLabel"}` | byte-identical builders, unit-tested against the C's `sb_add` sequence |
| error codes | `-32003`, `-32004`, `-32005 StaleHandle`, `-32006 UnknownClass`, `-32017 PrerequisiteFailed` (bad redo history), `-32018`, `-32022`, `-32026 ConnectionNotFound`, `-32602`, `-32603` (+ the `maybeExecuted` variant) | identical, with the C's `reason` strings and 512-byte `data` object |
| undo-group labels | `MCP: create entity`, `MCP: set keyvalues`, `MCP: set transform`, `MCP: delete nodes`, `MCP: add connections`, `MCP: remove connections` | identical strings |

## What each body does (and where the C is)

* `entity.create`: `entity_create_target`'s whole validation, then the class lookup in
  `CMapNodeFactory` (`:2101`), and only then `undo_open(doc, "MCP: create entity")` — the
  review-L3 rule that a refused create leaves no empty group. `CreateNodeByName`, the optional
  `SetScales` vtable write (slot 19, the C's `S2_BASEOFF`-derived layout), `handle_new`,
  `nodeID`, the optional `keyValues` echo through `apply_keyvalues(..., with_previous = 0)`.
* `entity.setKeyValues`: `set_keyvalues_target` validates the handle/nodeID form, the node, the
  lock and `values` **before** the group opens (`:5834`), then one `RecordNodeTree` on the
  active history, the two-pass `apply_keyvalues` (`previous` first, then the writes) and
  `DocSetModified`.
* `entity.addConnections`: `conn_array_parse` (1..64 entries, every rule), the node, the lock,
  the current vector's count and the room check (`count + n <= 512`) all before the group; then
  the per-entry "Add New Output" recipe — an 80-byte `S2ConnectionSrc`, `targetType = 7`,
  `Kv3Init(&paramMap, 1, 8)` / `AddConnection` / `Kv3Free` — and the read-back
  (`*outId == index << 16`, count grew by exactly one, the entry reads back).
* `entity.removeConnections`: one of `indices` / `match` resolves to a live index set before the
  group; removals run in descending index order; an `AddConnection`-style refusal after the
  group opened is the C's `op_err_maybe`.
* `node.delete`: up to 256 whole i32 ids, then every node resolved/class-checked/unlocked before
  the group; one `MCP: delete nodes` group; `DeleteNode` per id.
* `node.setTransform`: the whole param set validated before the group; one `RecordNodeTree`; the
  C's own write order **scales, angles, origin** (slot 19/18/17); the reply reads all three back.
* `undo.undo` / `undo.redo`: the history's own group count (`CMapDocHistory_Count`) decides
  emptiness (never `HasEntries`); the popped group's label is read before the pop, the depth
  after; redo additionally requires `obj_is(GetRedoHistory(doc), HV_HISTORY_VT)`.

## Places the port deliberately differs from the C (no byte on the wire)

1. **The write path is split host-side/Windows-side.** The parameter validation, the
   `applied`/`warnings`/`previous` decisions and every result builder are plain functions over
   strings, numbers and small structs, and the game access is one `Write` trait method per C
   primitive (`ops::hammer::Write`). The host tests drive them through a recording fake, which
   is how every param error and every builder is asserted byte for byte without a game. The
   Windows implementation (`ops::hammer::win`, `impl Write for LiveHammer`) binds each method to
   the generated vtable slot or the pinned `HFN_*` entry exactly as the C's `g_hfn` call does.
2. **`doc_tail_json` (m3.2) is replaced by `doc_tail_into`**, and `entity_json::Sb` gained
   `truncate` (`mcp_bridge.c:1883`'s `out->len--`). The bytes are the C's; the difference is
   only that there is no loose "append this text" helper that could be spliced at the wrong
   place. m3.2's diff item 3 described the old shape.
3. **The attribute pointer is carried as an opaque token.** `attr_info` answers the C's
   `CDmAttribute *` (plus `sym.m_pString`/`mem_ok`/`typeFlags` facts) and the int/float setters
   receive that token, so `SetValueInt`/`SetValueFloat` are called on the same attribute the C's
   single `FindAttribute` resolved. `attr_previous` interns and `FindAttribute`s in its own
   pass, exactly as the C's `previous` pass does (`:2320-2324`). One `GetEntityProperties` per
   op, as in the C (`:2372`, `:2208`). No wire difference; recorded because the trait boundary
   makes it visible.
4. **Guarded reads where the C dereferences unguarded**: `kv_case_variant`'s chain walk
   (`:2029-2037`) re-probes each attribute, the `attr_info` `typeFlags` read is gated on
   `mem_ok` (the C reads it after the same check), and `case_variant`'s answer is `probe_str`
   capped at 96 bytes — exactly the C's `kv_warn_variant` copy. `properties` (the write path's
   `GetEntityProperties`) keeps the read lane's `mem_ok` probe, where the C's
   `set_keyvalues_target` (`:2520`) only checks for null. A readable attribute or element
   produces the same bytes; an unreadable one answers "no variant" / `-32603
   "entity_properties missing"` instead of faulting.
5. **`history_info` for the write replies** is the same reader the read lane ported; the C's
   unguarded `Groups[n-1]` read stays guarded (m3.2 item 3 remains the record of that).
6. **The `-32003`/`-32018`/`-32004`/`-32017` data objects** are built by `OpErr::new` /
   `OpErr::with_data` in the 512-byte buffer, where the C's `geo_dispatch` uses a 192-byte
   scratch. All these reasons are short (`no_session`, `inactive`, `missing`, `doc_id`,
   `epoch`, `missing_id`, `missing_epoch`, `bad_redo_history`), so both cut at the same byte.
7. **A `kv: parser stalled` early return** is modelled as `KvApply::stalled` and rendered as the
   arm's `-32603` overflow error (the C sets `out->ovf = 1` and returns, and the arm's overflow
   tail answers that same error). `kv_next` provably advances, so no live frame reaches it. The
   stall is logged exactly once, by the pass that hits it (`apply_keyvalues_with`), where the C
   logs it (`:2221`, `:2242`); `kv_apply_into` only propagates the overflow.
8. **Non-Windows builds have no writer.** On Linux the module, the validation and the
   registration exist (the gate answers `-32017 hammer.dll`), and every write body answers
   `-32003 NoActiveDocument hammer_not_loaded` — never a fabricated success. `tests/ops_hammer.rs`
   asserts exactly that difference.
9. **`entity.addConnections`' connection-spec scratch is heap-allocated.** The C keeps it
   `static ConnSpec specs[CONN_MAX]` (`:3724`) in BSS; the Rust op builds the same `CONN_MAX`
   entries in a `Vec`, because the body runs on Hammer's main thread stack and the array is
   ~66 KB. Same size, same parse order, no byte on the wire.

## Live parity (`bridge/*.py` under Wine against the Rust bridge)

Covering this group's rows:

* `bridge/test_entity_create.py` — the `entity.create` sequence: `doc.active` baseline,
  create, `map.save`, `doc.active` after (undoDepth/undoName), the saved `.vmap` check. It
  exercises the create reply's `handle`/`nodeID`/`class`/`origin` and the `MCP: create entity`
  group the reply names.
* `bridge/test_set_keyvalues.py` — `entity.setKeyValues` end to end: `applied`, `warnings`,
  `previous`, `previousTruncated`, `valueUndo`, and the `SetKeyValue` path that makes a prop
  model load.
* `bridge/test_undo.py` — `undo.undo`/`undo.redo`: the drain loop (empty stack →
  `{"undone":false}`), group order for create + setKeyValues, the depth/name fields.
* `bridge/test_props.py` — the `entity.get` view of every write (keyValues, live connections):
  a wrong `applied`/`previous` or a wrong connection shows up there.
* `bridge/test_triggers.py` — `entity.addConnections` / `entity.removeConnections` /
  `entity.tieMeshes`: the connection recipes, the read-back, the `landed` list and the removal
  counts.
* `bridge/test_geometry_undo.py`, `bridge/test_geometry.py` — `node.setTransform` /
  `node.delete` through the geometry checks and their undo round trips.
* `bridge/test_edit_batch.py` — `edit.batch` composes `entity.create`/`entity.setKeyValues`
  across languages; it is the strongest end-to-end check of these two bodies (and stays
  `-32011` in this build, so the batch half still answers from the C bridge semantics).
* `bridge/test_client.py` — `caps_*`: the eight rows are advertised once hammer.dll verifies.

`--static` modes: `bridge/test_entities.py --static` greps the **C** source for the read rows
(`static_rows_implemented`, `static_rows_not_blocked`, `static_readonly_dispatch`,
`static_nodeid_branch`); a Rust counterpart would have to grep `ops::hammer::HANDLERS` and
`registry::METHODS` instead. No write-row `--static` check exists in the scripts; the Rust
tests that cover the same ground are the `ops::hammer::tests::*` builder/param tests and
`tests/ops_hammer.rs` (the `-32017` gate for all twelve rows, including a proof that the bodies
themselves answer `-32003` on a host without hammer.dll).

## Negative evidence (revert one thing at a time)

* a write row removed from `ops::hammer::HANDLERS`: `ops::tests::every_handler_...` still
  passes, `tests/ops_hammer.rs::the_ported_rows_are_advertised_once_hammer_verifies` fails (the
  row vanishes from `capabilities.methods`), and the row answers
  `-32011 "rust bridge m3.0: not ported yet"` again.
* `write_gate`'s preconditions reordered (expectDoc before `doc_editable`): a request to an
  inactive document with a stale `expectDoc` answers `-32004` where the C answers `-32018`
  (`ops::hammer::tests::the_write_gate_is_the_c_check_order`).
* the room check in `op_entity_add_connections` moved after `undo_open`:
  `ops::hammer::tests::add_connections_refuses_to_cross_the_read_back_cap_before_the_group`
  fails (an empty `MCP: add connections` group is left behind).
* `remove_connections_into` printing the walk order instead of reversing it:
  `ops::hammer::tests::remove_connections_removes_descending_and_reports_ascending` fails.
* `undo_step`'s emptiness test switched to `HasEntries`: a history whose own count is 0 but
  whose partner holds groups reports `undone:true` where the C answers `undone:false`.
