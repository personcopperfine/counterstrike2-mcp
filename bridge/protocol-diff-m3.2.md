# Protocol changes in m3.2 — lane HAMMER-R (`doc.active`, `node.list`, `entity.get`, `entity.find`)

Bridge build `m3.2` (Rust bridge) = m3.1 + this group.

**One row group moved from "not ported" to served.** The Rust bridge now answers the four
read-only Hammer methods byte-for-byte like the C bridge at m2.20. Nothing else changed:
`BRIDGE_BUILD` stays `m3.0` (the orchestrator bumps it at merge), the method table is unchanged
(84 registered / 69 implemented), no new error code, no changed field.

## The wire, field by field

| Where | m2.20 (C bridge) | this build |
|---|---|---|
| `capabilities.methods` | every implemented, unhidden row whose needs are verified | the same, **intersected with the ported list**: `control.*` + `doc.active`, `node.list`, `entity.get`, `entity.find` (table order) |
| `doc.active` result | `{"doc":null,"reason":<hop>}` or the full document object (`mcp_bridge.c:6234-6277`) | identical bytes; the envelope is built in the same one-shot `MAX_LINE` buffer, so an overflow cuts at the same byte and logs `exec: response buffer overflow` |
| `node.list` result | `{"nodes":[...],"count":N,"visited":N,"truncated":b,"source":"live"}` (`:5512-5538`) | identical (walks, class filter, `limit` clamp 1..10000 → 2000) |
| `entity.get` result | `{"nodeID":…,"classname":…,"targetname":…,"origin":…,"angles":…,"scales":…,"keyValues":{…},"keyValueCount":N,"unsupported":[…],"truncatedValues":[…](,"truncated":true)(,"connections":…)(,"children":…),"source":"live"}` (`:3694-3838`) | identical |
| `entity.find` result | `{"entities":[{"nodeID","classname","targetname","origin"}],"count":N,"visited":N,"truncated":b,"source":"live"}` (`:4565-4668`) | identical |
| error codes | `-32003 NoActiveDocument`, `-32022 NodeNotFound`, `-32602 InvalidParams`, `-32603 InternalError` + the six `reason` strings of `doc_editable` | identical |

## What the four bodies do

* `doc.active`: `active_doc` → `doc_fingerprint` → the document object. The fingerprint
  (`g_fp`, `:1736-1779`) is sampled once per tick from the same place the C samples it
  (`mcp_bridge.c:6819`; the Rust tick's `modules::live().poll()` then
  `crate::ops::hammer::doc_fingerprint()`), and again inside the reply, so `id`/`epoch` are
  current. Like the C, the tick only samples once `g_hammer` was resolved (`:1747`), so a
  `doc.active` first call reports the same epoch numbers the C does.
* `node.list` / `entity.get` / `entity.find`: `geo_dispatch`'s head (`:5719-5746`) = resolve the
  live document (or `-32003` with the hop's reason), then the op. Reads never run the
  `doc_editable`/`expectDoc` arms (those are `mutating` only).
* The whole `g_hfn` bind table is bound from the generated `HFN_*` pins
  (`bridge/s2sdk.h:698-742`), including the entries no read calls yet (the write lanes).

## Places the port deliberately differs from the C (no byte on the wire)

1. **Unguarded game reads got `mem_ok` guards** where the C dereferences unguarded and would
   fault on a corrupt pointer: the `CMapMesh_PolyMesh` component counts (`:3207-3208`), the
   undo group pointer (`:1817`), the entity-properties attribute chain end, and the doc path
   copy (`:6245`, the C's `sb_addjson(&b, pathp)` reads an unvalidated `char *`).
   Readable inputs produce the same bytes; unreadable ones produce the C's `null`/`omitted`
   shapes instead of a crash.
2. **The doc path is copied through `probe_str` with a 4096-byte buffer** (the C copies
   unbounded into its 1 MB reply). A path longer than 4095 bytes is truncated here where the C
   would emit it whole; CUtlString paths are MAX_PATH-sized, so this is unreachable in
   practice. Kept deliberately after review R2 #7 (the alternative - an unbounded game-read in
   a `sb_addjson` - is what every other guarded read in this module avoids); the cap is stated
   in `ops::hammer::win`'s `doc_facts` where it happens.
3. **`sb_add_doc_object` / `sb_add_doc_tail`** are ported as `sb_add_doc_object` (writes into a
   builder, unchanged) and `doc_tail_json` (returns the `,"doc":{…}` text). The C's
   `sb_add_doc_tail` rewinds the result's last byte, which `entity_json::Sb` cannot do; the
   bytes are identical when a write lane appends the returned text before its closing `}`.
   No read carries a `doc` tail, so this is only visible to the next M4 wave.
4. **`resolve_hammer`'s module-handle check** compares `GetModuleHandleW("hammer.dll")` with
   the verified base directly, because `s2sdk::module::Module` exposes no base accessor (its
   `ImageRange` keeps the base private). Every other identity check is the C's:
   `PIN_HAMMER` verified, the handle equality, the `CreateInterface("ToolSystem2_001")` return
   code, the `HG_HAMMERAPP_OBJ` address, `obj_is(HV_HAMMERAPP_VT)`.
5. **`-32003`/`-32018`/`-32004` data objects**: the C builds them in a 192-byte scratch
   (`geo_dispatch:5725`); `OpErr::new` uses the 512-byte `OpErr.data` buffer
   (`op_err:1990`). The reasons are the C's short strings (`no_session`,
   `doc_session_backptr`, …), so both truncate at the same byte.
6. **Non-Windows builds have no reader.** `hammer.dll` is a Windows module and the generated
   `hammer`/`mapdoclib`/`toolutils2` bindings are `cfg(windows)`; on Linux the
   `ops::hammer` module, its builders, its validation and its registration still exist (the
   gate answers `-32017 hammer.dll` exactly as it would on Windows with the module missing),
   but any node read fails loudly instead of inventing an answer. The live parity pass runs
   the Windows build, which is the one that matters.

## Round 2 review fixes (base `7ad89f5`)

All eight findings, and what each one costs on the wire:

1. **`entity.get`'s two name lists have their own 1024-byte builders** (`mcp_bridge.c:3752-3755`),
   and an overflow of either fails the whole op (`:3834` `out->ovf` → `-32603 "entity has too
   many or too large keyvalues"`, `geo_dispatch:5774-5775`). Previously the lists were emitted
   from the result builder, so a list past 1024 bytes was reported as a result overflow
   (`-32603 "entity has too many or too large keyvalues"` too, but only once the 512 KB reply
   was exhausted, i.e. never). The new behaviour is the C's for every input; covered by
   `ops::hammer::tests::entity_get_name_lists_overflow_their_1024_byte_builders`.
2. **A negative child `nodeID` prints `null`** (`:3682`), in `children_into` - the C's
   `node_id_of` leaves the id at -1 on failure and the reply must never name a node a reader
   would address. Covered by `a_negative_child_node_id_prints_null_like_the_c`.
3. **Game text is carried as raw bytes** (`Vec<u8>`) in `EntityRead`, `Conn`, `DocFacts`,
   `HistoryInfo` and `probe_text`, so `Sb::addjson` performs the C's invalid-UTF-8 repair
   (`sb_addjson`, `:292-319`) instead of a lossy conversion happening first. A game string with
   invalid UTF-8 now reaches the wire as the C's one-U+FFFD-per-maximal-subpart bytes; covered
   by `invalid_utf8_game_text_becomes_one_replacement_character_as_the_c` (`ED A0 80`).
4. **`elem_attr` compares the attribute name byte by byte with the C's early exit** (`seq`,
   `:3288`) rather than `CStr::from_ptr` (which walks the whole game string before comparing);
   covered by `the_attribute_name_compare_exits_at_the_first_difference`.
5. **`m_bActive` and `m_bIsNew` are read as one byte** and only their truthiness crosses
   (`:6246`, `:6248`); no Rust `bool` is ever formed from game memory. Covered by
   `a_game_flag_byte_is_true_for_any_nonzero_value`.
6. **The child/undo/connection vectors are read through raw pointers and `read_unaligned`**,
   never through a `&CUtlVector_…` (there was a `&'static`) taken before the `mem_ok` guards;
   the connection element and the undo group label are copied out by value. The SAFETY note at
   the undo group slot now says what is true: only `groups[0]`'s address is proven, and the C
   reads `Groups[n-1]` unguarded too (`:1817`) - this port additionally probes the one slot it
   reads.
7. **The `doc.active` path cap stays 4096** (see "Places the port deliberately differs" #2
   above); the comment at the cap names the difference and this file records it.
8. **No hand-written strides or offsets left in this module's reads**: the child handle stride
   is `size_of::<u32>()` (`:3369`, `:3587`), and `node_vec` takes `z`'s address from the
   generated `Vector`/`QAngle` field instead of `start + 8` (`:3290-3292`).

## Live parity (`bridge/*.py` under Wine against the Rust bridge)

Covering this group's rows:

* `bridge/test_entities.py` — the full `entity.get`/`entity.find`/`node.list` pass, including
  `--static` (offline): `static_rows_implemented`, `static_rows_not_blocked`,
  `static_readonly_dispatch`, `static_nodeid_branch`. Its `--static` mode greps the **C**
  source; the Rust counterparts of those four checks are `ops::hammer::tests::*`
  (builder bytes, param errors, walk output) and the `ops` self-test
  (`every_handler_is_an_implemented_gated_row_registered_once`).
* `bridge/test_props.py` — `entity.get`'s `keyValues`/`unsupported`/`truncatedValues` and the
  live `connections` vector read-back.
* `bridge/test_client.py` — `doc` (was "Fail, by design" in m3.0) and `caps_*`: with the four
  rows served, `capabilities.methods` reports them and `doc.active` answers a document object.
* `bridge/test_undo.py`, `bridge/test_set_keyvalues.py`, `bridge/test_edit_batch.py`,
  `bridge/test_entity_create.py`, `bridge/test_triggers.py` — they read `doc.active` /
  `entity.get` for their own setup or verification, so a wrong reply shows up there too.
* `bridge/test_geometry.py`, `bridge/test_reload.py` — `node.list` tree walks.

## Negative evidence (revert one thing at a time)

* `mod hammer` removed from the `ops::GROUPS` table: `ops::tests::every_handler_...` still
  passes but `tests/ops_hammer.rs::the_ported_rows_are_advertised_once_hammer_verifies` fails
  (the four rows vanish from capabilities) and the C's rows answer
  `-32011 "rust bridge m3.0: not ported yet"` again.
* the gate's needs check moved after the body lookup: `tests/ops_hammer.rs::an_unverified_
  hammer_fails_closed_before_any_body` fails (the body answers `-32003`/panics instead of
  `-32017 hammer.dll`).
* `doc_fingerprint` removed from the tick: `doc.active`'s `epoch` only advances
  inside its own reply, so a document change observed between two reads stops bumping `epoch`
  where the C's per-tick sampling bumps it (the C's `:6819` order).
* `probe_str`'s page re-probe removed: `ops::hammer::tests::probe_str_copies_or_cuts_a_game_
  string` still passes for local buffers but a game string straddling a page boundary would be
  read past the mapped page (the C's `:1980` guard is what makes it safe).
