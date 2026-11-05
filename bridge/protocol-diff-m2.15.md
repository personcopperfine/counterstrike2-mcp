# Proposed protocol changes for bridge m2.15 (Hammer entity pass: live reads by nodeID, string coercion)

Additive over m2.14; no major bump, **no new error codes**. Two documented-but-blocked rows become
real (`entity.get`, `entity.find`), `entity.setKeyValues` gains a `nodeID` form next to `handle`,
and `apply_keyvalues` coerces the JSON the Rust layer sends onto the DME **String** attributes a
saved `.vmap` actually holds (`vmap-format.md` §4: 43 399 String vs 28 Element; `defuse_poc.vmap`
nodeID 30 has `"bombradius": "500"`, `"buying": "0"`). Sources read for this pass:
`target/entities-plan.md` §3.1-3.5/§4.1 WP-B, `mcp_bridge.c` (`elem_attr`, `geo_walk`,
`node_id_of`, `node.delete`'s `NodeIsLocked`, `entity.create`'s `apply_keyvalues`),
`idabins/docs/data/CMapEntity.md` and `hammer-undo.md` §C. Consumer: `hammer-mcp`'s
`get_entity_live` / `find_entities_live` / `set_entity_keyvalues` (WP-T2a/T2b) behind the existing
capability gating; nothing in Rust ever sees an address.

**Status.** Everything below is `[I: offline]`: `./bridge/build.sh` compiles it warning-free and
`bridge/test_entities.py --static` pins the rows, the `nodeID` branch, the coercion and the result
contract offline. The live promotion (V1-V4, plan §5 steps 1-12) is left to the orchestrator, which
runs `bridge/test_entities.py` against an open `defuse_poc.vmap` and then records the runtime
evidence in these rows — the bridge cannot claim runtime evidence for itself, so nothing here does.

**Modality note.** `entity.get` and `entity.find` are read-only: they walk the live tree exactly the
way `node.list` does (depth 64, `GEO_MAX_NODES`, the same `mem_ok` guards) and never take a lock,
open an undo group or touch the document, so they run through `geo_dispatch(..., mutating=0)` (no
`doc_editable`, no `expectDoc`). `entity.setKeyValues`'s `nodeID` form opens the same
`MCP: set keyvalues` group the `handle` form opens (`geo_dispatch`/the standalone arm is
unchanged), and it keeps the `-32018 DocumentNotEditable` refusals — a locked node fails the same
way `node.delete` fails it.

## §7 method table — changed rows

| Method | Status | Bridge m2.15 | Params | Result | Call | Notes |
|---|---|---|---|---|---|---|
| `entity.get` | implemented `[I: offline]` — `./bridge/build.sh` + `bridge/test_entities.py --static` pass; the orchestrator's `bridge/test_entities.py` (plan §5 steps 1-4) promotes it | gate `NEED_HAMMER`, read-only | `{nodeID:int >= 0}` | `{nodeID:int, classname:string, targetname:string, origin:[x,y,z], angles:[x,y,z]\|null, keyValues:{name:value}, keyValueCount:int, unsupported:[name,...], truncatedValues:[name,...], connections:null, truncated?:true, source:"live"}` | `geo_find(doc, nodeID)` (the `node.delete`/`mesh.*` lookup); `obj_is(node, HV_MAPENTITY_VT)`; `GetEntityProperties(node)`; walk `child->attrs` with `elem_attr`'s linked-list guards and emit by type: 5 String -> the `CUtlSymbolLarge` the storage holds (the `char*` `dm_intern`/`SetValueString` write), read through `probe_str` (per-page `mem_ok` probe, 4096-char cap); 2 int / 3 float / 4 bool -> the typed setters' value storage; anything else -> `null` + the name in `unsupported`; `node_origin` + `HOFF_NODE_ANGLES` for the vectors | `nodeID` is read as a TOP-LEVEL plain integer (`json_int_top`): a nested `nodeID` keyvalue, a fraction or an id above `2147483647` is `-32602` (`nodeID required` / `nodeID: a non-negative int up to 2147483647 is required`), never a truncated `(int)` cast onto another node (review opus #5 / ds #1). Skip an attribute literally named `id`. `angles` is `null` when the node's angle storage is unreadable (never an invented offset). `classname`/`targetname` are the child's attributes as text (`""` when it has none). The attribute walk is capped at 512 emitted keyvalues, then `truncated:true`. A String longer than 4096 bytes is `null` and its name lands in `truncatedValues` (never a cut string the caller could treat as the value - review opus #6/#9). `connections:null` is honest, not a guess: the `connectionsData` element-array layout is not pinned, so I/O connections come from the file route (`get_entity`) only. Result buffer: the 512 KB `geo_dispatch` scratch; its overflow is answered `-32603 entity has too many or too large keyvalues` (review opus #17: `entity.get` has no `limit` param), and an overflow of the `unsupported`/`truncatedValues` name lists fails the op the same way (review ds #5). `-32602 InvalidParams` (`nodeID required`, `nodeID: a non-negative int up to 2147483647 is required`, `not an entity`), `-32022 NodeNotFound`, `-32603 InternalError {reason:"entity_properties missing"}`. |
| `entity.find` | implemented `[I: offline]` — same promotion | gate `NEED_HAMMER`, read-only | `{class?:string, targetname?:string, within?:{origin:[x,y,z], radius:number>0}, limit?:int 1..2000 (default 500)}` | `{entities:[{nodeID:int, classname:string, targetname:string, origin:[x,y,z]}], count:int, visited:int, truncated:bool, source:"live"}` | walk like `node.list` (depth 64, `GEO_MAX_NODES`), keep `HV_MAPENTITY_VT` nodes, read `classname`/`targetname` from `GetEntityProperties` with the symbols interned once per call, filter case-insensitively (exact match, `ci_seq`), then the sphere test on the node origin (`d2 <= radius^2`) | Filters are read as TOP-LEVEL members (`json_find_top`), so a nested `class`/`targetname` in another object is not a filter (review opus #1). No filter lists everything up to `limit` (default 500); an empty `class`/`targetname` string is `-32602 {reason:"class: a non-empty string is required"}` instead of match-all (review opus #15) and a `limit` outside 1..2000 is `-32602 {reason:"limit: an int in 1..2000 is required"}` instead of a silent clamp to the default (review ds #11). `truncated:true` when more entities matched than fit; `visited` counts every node walked (as `node.list` reports it). `-32602 InvalidParams {reason:"within: {origin:[x,y,z], radius > 0} required"}` for a malformed `within` (a `radius`/`origin` in exponent form like `1e20` parses - review opus #8); `-32603 InternalError {reason:"no world node"}`. Result buffer: the same 512 KB `geo_dispatch` scratch. |
| `entity.setKeyValues` | extended `[I: offline]` — the `nodeID` form and `previous`/`valueUndo` need the orchestrator's V3/V4 run | gate `NEED_HAMMER` | `{handle?:string, nodeID?:int >= 0, values:{name:value,...}, expectDoc?}` — **exactly one** of `handle`/`nodeID` (`-32602 InvalidParams` with `handle and nodeID are mutually exclusive` / `handle or nodeID required` when both or neither) | `{applied:{name:bool,...}, warnings:[string,...], previous:{name:value\|null,...}, previousTruncated:[name,...], valueUndo:"group-only"}` | `handle` path unchanged (`handle_lookup` -> `-32005 StaleHandle`); `nodeID` path: `geo_find` -> `-32022 NodeNotFound`, `obj_is(node, HV_MAPENTITY_VT)` else `-32602 {reason:"not an entity"}`, `NodeIsLocked` -> `-32018 DocumentNotEditable`; then `GetEntityProperties` -> `-32603 {reason:"entity_properties missing"}` and `apply_keyvalues(child, values, out, with_previous=1)` | `handle`, `nodeID` and `values` are read as TOP-LEVEL members (`json_string_top`/`json_int_top`/`json_find_top`): a nested `nodeID` (e.g. a keyvalue of that name inside `values`) is not the op's nodeID, and a `nodeID` above `2147483647` or a fraction/exponent lexeme is `-32602 {reason:"nodeID: a non-negative int up to 2147483647 is required"}` instead of a truncated `(int)` cast that would write another node (review opus #1/#5, ds #1). `previous` is read in a pass of its own **before any attribute is written**, one entry per requested name: the attribute's value as JSON text (String -> its text, int/float -> the number, bool -> `true`/`false`) or `null` when the entity does not have the attribute. `valueUndo:"group-only"` is the documented undo gap: `undo.undo` pops this group but does **not** revert attribute values, because the setters push `CUndoAttributeSetValueElement` into the CDataModel scope, which the `CMapDocHistory` group does not own (`m4-undo.txt`; `entity_properties` is a plain `CDmElement`, so `RecordNode*` has no `IUndoable` to record - do not try it). Callers revert by re-applying `previous`; a `previous` value longer than 1024 bytes is `null` **and** its name is in `previousTruncated`, so a caller never re-applies a cut value (review opus #6). A requested value longer than 1024 bytes is `applied:false` with the warning `"<name>: value longer than 1024 bytes"`, and the keys after it still parse (review opus #7, ds #3). A refused call (bad params / node not found / not an entity / locked node / missing `values` / missing child) resolves nothing and opens **no** undo group, so no empty "MCP: set keyvalues" group is left for `undo_last` to pop (review opus #13). Batch: unchanged (`entity.setKeyValues` was already whitelisted and the `$N` handle substitution still applies); `values.classname` stays refused on the Rust side, the bridge does not special-case it. Standalone result buffer 65536 bytes; an overflow is answered `-32603 response too large (use fewer keyvalues)` instead of a cut JSON line. |

## §3.4 coercion (shared by `entity.create` and `entity.setKeyValues`)

`apply_keyvalues` no longer creates an attribute of the JSON value's type and no longer reports
`applied:false` for the normal case:

- an existing **String** attribute takes the JSON token's own text: a JSON string as-is, a number as
  the literal text the caller wrote (`kv_next` now returns the raw token — `4` stays `"4"`, never
  `"4.000000"`), a bool as `"1"`/`"0"`. `applied:true`.
- a key the entity does not have is **always created as `S2_AT_STRING`** and written with the text
  (never an int/float/bool attribute — no real `.vmap` has one of those for a keyvalue). The Rust
  layer sends strings for typed FGD keys, so the type information stays lossless in the file.
- an existing int/float/bool attribute keeps today's typed setters (`SetValueInt`/`SetValueFloat`)
  and today's kind check, and logs the mismatch as before.
- any **other** existing attribute type is not written: `applied:false` **and** a real entry in
  `warnings`, `"<name>: attribute type <n> not writable"` (m2.14 always sent `"warnings":[]`). The
  array is now built for real; the converter caps it rather than overflowing the reply.
- a value longer than `KV_VALUE_MAX` (**1024 bytes**, Rust caps at 512) is not written:
  `applied:false` **and** `"<name>: value longer than 1024 bytes"`, and the keys after it still
  parse (review opus #7, ds #3: a 512-byte value used to be stored as 511 bytes with
  `applied:true`). A `previous` value longer than the cap is `null` and its name lands in
  `previousTruncated`, never as a cut string (review opus #6).
- the attribute pointer is `mem_ok`'d before `typeFlags`/`value` are read; an unreadable attribute
  is `applied:false` with `"<name>: attribute not readable"` (review ds #4).
- a JSON string value decodes `\uXXXX` (surrogate pairs included), `\b`, `\f` and `\/` (review
  opus #16), and a number lexeme keeps its raw token text (`1e20` stays `"1e20"`).
- an **empty** string value (`""`) is written like any other text and answers `applied:true`
  (m2.16 fix): it interns to a `CUtlSymbolLarge` with `m_pString == NULL` — Hammer's own
  representation of the empty string, the one `attr_cstr`/`entity.get` read back as `""` — and the
  bridge now passes that symbol to the setter unconditionally. The setter
  (`CDmAttribute::SetValueString`, `HFN_SET_STRING`) only stores the pointer, and Hammer's own
  `CDmElement` string setter interns through the same `GetSymbol` and passes the result on with no
  null check, so a NULL symbol is exactly what Hammer writes. Before m2.16 the write was skipped by
  an `if (vsym.m_pString)` guard, which reported `applied:false` with **no** warning at all.

## §8 error codes — no new codes

m2.15 adds **no** error code. Reused, with the reason strings above: `-32602 InvalidParams`
(`nodeID required`, `nodeID: a non-negative int up to 2147483647 is required`, `not an entity`,
`handle or nodeID required`, `handle and nodeID are mutually exclusive`, `values`, `within: ...`,
`class: a non-empty string is required`, `targetname: a non-empty string is required`,
`limit: an int in 1..2000 is required`, `origin: [x,y,z] required`,
`angles: [pitch,yaw,roll] required`), `-32022 NodeNotFound`, `-32018 DocumentNotEditable` (locked
node), `-32005 StaleHandle` (handle path), `-32603 InternalError` (`entity_properties missing`,
`no world node`, `response too large`, `entity has too many or too large keyvalues`).

## §5/§6 notes

- Method count: **78 registered / 62 implemented** (m2.14: 78/60; the two flips are `entity.get`
  and `entity.find`, both `impl: 1` now). `control.version.bridgeVersion` and every `"build"` field
  are **`m2.15`**.
- **No new pins, no new vslots, no new RVAs.** `PIN_MODULE_COUNT` stays 12 and
  `bridge_pins.h`/`sigs/` are untouched: the new code reads only objects the existing pins already
  describe (`HV_MAPENTITY_VT`, `HV_DATAMODEL_VT`, `HOFF_NODE_ANGLES`, `HG_DATAMODEL`,
  `HFN_GET_ENTITY_PROPERTIES`, `HFN_FIND_ATTRIBUTE`, `HFN_CREATE_ATTRIBUTE`, `HFN_SET_STRING`,
  `HFN_NODE_IS_LOCKED`). `bridge/s2sdk.h` is untouched too: the `angles` field the plan asked about
  is already declared with its `S2_FIELD(HOFF_NODE_ANGLES)` assert, so no struct change was needed.
- Like every other pin/protocol number, the new code has **no** numeric literal: the diff against
  the base branch introduces no `0x` literal, which `bridge/test_entities.py --static` checks
  (`git show hammer/entities:bridge/mcp_bridge.c` vs the working file, added lines only). A missing
  base ref fails the test loudly instead of skipping.
- Reuse, not a new walk: `entity.get` resolves through the existing `geo_find`, and `entity.find`
  copies `geo_walk`'s descent/guards (depth 64, `GEO_MAX_NODES`, `childHandles` bounds,
  `mem_ok` per page) rather than touching `node.list`'s `GWalk`. The entity type check is
  `obj_is(node, PIN_HAMMER, HV_MAPENTITY_VT)`, the same identity `node.delete`/`node_class` use.
- The Version bump is the only change to files outside the new code:
  `bridge/test_modeldoc.py` (`--static` build string), `test_modeldoc_render_live.py`, `test_pet.py`
  and `test_pet_live.py` assert the bridge build and are bumped to `m2.15` in this pass (m2.14's diff
  note says the same about m2.13 -> m2.14).
- Live probe list for the orchestrator (`bridge/test_entities.py`, plan §5 steps 1-4): the version
  and the two advertised methods; `entity.get{nodeID:30}` on the open `defuse_poc.vmap` ->
  `classname:"info_map_parameters"`, `keyValues.bombradius == "500"`, `unsupported == []`, the whole
  `keyValues` map equal to `python3 -I bridge/vmapreader/vmap_dmx.py defuse_poc.vmap get 30 --json`,
  `origin` equal to the file's; `entity.find{class:"func_buyzone"}` -> nodeIDs `{67, 70}` with the
  file's origins; `entity.get` on a mesh nodeID -> `-32602 not an entity`, on a missing nodeID ->
  `-32022`, `setKeyValues` with both `handle` and `nodeID` -> `-32602`; `setKeyValues{nodeID:30,
  values:{bombradius:501, vscripts:"x_<run>"}}` -> `applied` both true,
  `previous.bombradius == "500"`, `valueUndo:"group-only"`, `map.save`, the file shows `"501"` and
  `vscripts` as **strings**; `undo.undo` -> the group `MCP: set keyvalues` popped and `entity.get`
  **still** shows `501` (the recorded gap), then the test restores from `previous`, saves and the
  file shows `"500"` again. V1/V2 (the child for a file-loaded entity, the string read matching the
  file) are the two STOP items of plan §3.6 and are covered by step 2.

## Review fixes (FIX-B, 2026-10-14) — `[I: offline]`

A read-only review pass over this change (`target/review-opus.md`, `target/review-1.md`) found the
defects below in the m2.15 code; all of them are fixed in `bridge/mcp_bridge.c` and pinned offline
(`./bridge/build.sh`, `bridge/test_entities.py --static`, the new host unit test
`bridge/test_entity_json.c`). **Nothing here is live-verified**: the runtime behaviour of these
paths stays `[I: offline]` until the orchestrator re-runs `bridge/test_entities.py` (plan §5) - the
fixes touch only the parameter parsing, the value/limit caps and the error paths, so the live
observations of the 2026-10-14 run above still describe the same happy path.

1. **Op params are read at the top level only** (review opus #1, ds #1). `json_find` returns the
   first match anywhere in the text and serde_json writes the op params in key order, so
   `keyValues.origin` (a string) came before the real `origin` array: `entity.create` placed the
   entity at [0,0,0] and wrote a stray `origin` string keyvalue. New `json_find_top` plus
   `json_string_top`/`json_int_top`/`json_float3_top`/`json_float1_top` match TOP-LEVEL members
   only (nested objects/arrays and string contents are skipped), and the entity ops use them:
   `entity.create` (class/origin/angles/keyValues), `entity.setKeyValues` (handle/nodeID/values),
   `entity.get`/`entity.find` (nodeID/class/targetname/limit/within) and `expectDoc`. The
   `pet.*`/`modeldoc.*`/`game.*` arms keep `json_find`. A malformed `origin`/`angles` on
   `entity.create` is now `-32602` instead of a silently partly-zero vector.
2. **`edit.batch`'s op splitter is string/escape aware** (review opus #2): `batch_op_next` cuts on
   the new `json_bal_end`, which skips string contents, so a keyvalue containing `{`/`}` no longer
   ends the op early (that dropped the op's own `origin` and every following op). A truncated op
   object stops the walk instead of emitting a cut op.
3. **`nodeID` is validated as an i32 everywhere** (review opus #5, ds #1): an id above `2147483647`
   or a fraction/exponent lexeme is `-32602` in `entity.get`, `entity.setKeyValues`,
   `node.delete`'s `nodeIDs[]` and the mesh ops (`geo_entity_node`, `geo_target_mesh`,
   `NODEID_MAX`), instead of truncating through the `(int)` cast and reading or **writing** a
   different node. The Rust schemas keep `minimum: 0`; the bridge is the guard.
4. **No silent truncation of keyvalues** (review opus #6/#7, ds #3): the value buffer is
   `KV_VALUE_MAX` = 1024 bytes (Rust caps a keyvalue at 512). A longer value is `applied:false`
   with `"<name>: value longer than 1024 bytes"` and the rest of the object still parses; a
   `previous` value that does not fit is `null` plus the name in the new `previousTruncated`
   array; `entity.get` adds `truncatedValues` for the same reason (its `truncated` stays the
   attribute-walk cap). No caller is handed a prefix it could re-apply as the value.
5. **Exponent number lexemes parse** (review opus #8): `json_num` (behind
   `json_float1`/`json_float3` and their `_top` forms) and `kv_next` read `1e20`, `-1.5E-7`,
   `5.5e-17`; `kv_next` still returns the raw token text in `nval` for the string coercion.
6. **Frames stay valid UTF-8** (review opus #10): `sb_addjson` escapes control bytes as `\u00XX`,
   passes a valid multi-byte sequence through byte for byte and replaces an invalid or cut one
   with U+FFFD (one per maximal subpart); `probe_str` trims every cut with the new `utf8_prefix`,
   so a cut never ends inside a sequence and can no longer make the Rust client drop the frame.
7. **`kv_next` decodes `\uXXXX`** (surrogate pairs included), `\b`, `\f` and `\/` (review opus
   #16); an unknown escape keeps the escaped byte, as before.
8. **Overflow paths** (ds #2/#5, opus #17): the standalone `entity.create` arm checks `rb.ovf`
   (its result buffer grew 2048 -> 16384) and answers `-32603`; `entity.get`'s
   `unsupported`/`truncatedValues` name lists propagate their overflow to `out->ovf`; the
   `geo_dispatch` overflow text for `entity.get` says `entity has too many or too large keyvalues`
   (it has no `limit` param).
9. **`FindAttribute` results are `mem_ok`'d** before `typeFlags`/`value` are read in
   `apply_keyvalues`/`kv_previous_json` (ds #4); an unreadable attribute is `applied:false` with
   `"<name>: attribute not readable"`.
10. **`entity.find` refuses an empty filter** (`class`/`targetname` -> `-32602`, not match-all,
    review opus #15) and an out-of-range `limit` (`-32602`, not a silent clamp to the default,
    review ds #11).
11. **A refused `entity.setKeyValues` leaves no undo group** (review opus #13): the standalone arm
    validates the params and resolves the node (`set_keyvalues_target`) **before** `undo_open`, so
    a bad param / missing node / not-an-entity / locked node no longer leaves an empty
    `MCP: set keyvalues` group for `undo_last` to pop. The standalone `entity.create` arm had the
    same gap; it is closed in the re-review pass below (item 15).

**Offline proof.** `bridge/test_entity_json.c` is a host unit test in the `test_json_text.c`
style: `build.sh` lifts the marked `entity JSON region` out of `mcp_bridge.c` verbatim (the region
is freestanding - string builder, JSON scanners, `kv_next`, `batch_op_next`) into a generated
header, compiles the test against it and runs it before the DLL link, so it proves the shipped text
rather than a copy. 41 cases: the top-level lookup (`keyValues.origin` vs the real `origin`, a
nested `nodeID`, `expectDoc`), the string-aware splitter (`"a}b{c"` in a keyvalue), `kv_next`
(exponents with the raw token kept, `\uXXXX`/surrogate/`\b`/`\f`/`\/`, an overlong value with the
following key still parsed, the exact `KV_VALUE_MAX` boundary), `sb_addjson` (control bytes, valid
2/4-byte UTF-8, invalid/cut/overlong/raw-surrogate bytes -> U+FFFD) and `utf8_prefix`'s cut.
**Negative evidence (required):** with `json_find_top` replaced by the old depth-blind
`json_find`, the test fails `json_find_top_origin_beats_nested` (`top->"` instead of `top->[`,
`float3=0` -> the entity would land at [0,0,0]) plus the nested `nodeID`, `expectDoc` and
`json_string_top` cases (5 failures, `build.sh` exits 1); restored, all 41 pass and `build.sh`
exits 0.

## Live verification (2026-10-14, orchestrator)

`bridge/test_entities.py` against Hammer with `maps/ent_test.vmap` (a copy of `defuse_poc.vmap`),
bridge m2.15: **RESULT: PASS** (30 checks). These rows are now `[V runtime]`:

- `entity.get{nodeID:30}`: classname, origin and the full `keyValues` map equal the saved file
  (`vmap_dmx.py get 30`). Two fixes came out of the run: the DME element's own `name` attribute
  is skipped like `id` (it is not a keyvalue), and an empty string reads as `""` (an empty
  `CUtlSymbolLarge` is a null pointer).
- `entity.find{class:"func_buyzone"}` -> nodeIDs {67, 70}; mesh nodeID -> `-32602 not an entity`;
  unknown nodeID -> `-32022`.
- `entity.setKeyValues{nodeID}`: `applied`, `previous`, `valueUndo`; JSON `501` lands in the file
  as the string `"501"` after `map.save`.
- **Dirty flag:** the DME setters do not mark the map changed, so the first run's `map.save`
  answered `skippedClean:true` and wrote nothing. `entity.setKeyValues` now calls
  `CMapDoc::SetModified(doc, doc)` (new pin `HFN_DOC_SET_MODIFIED`, hammer!0x1808911C0, sig via the
  wrapper at hammer!0x180B82330, fingerprinted), the same call Hammer's own edit paths make after a
  change (hammer!0x18014EFB0). After it, `doc.active.dirty` is true and `map.save` writes the file.
- `undo.undo` pops `MCP: set keyvalues` and does **not** revert the value (known gap, unchanged).

## Re-review lows (N3/L6/L3 + the batch label, 2026-10-14) — `[I: offline]`

A second read-only pass over the m2.15 fixes (`target/orch/review-opus-2.md`) left four low items in
`bridge/mcp_bridge.c`; all four are fixed here. Still **no version bump** (`m2.15`), no new pins, no
new error codes and no new method. Nothing below is live-verified: they are parameter parsing, two
reader bounds and an undo-group label, so the live observations above still describe the same happy
path - the orchestrator re-runs `bridge/test_entities.py` (plan §5) to promote them.

12. **Integer params cannot wrap** (review N3). `json_int_at` (the `nodeID`/`limit` reader) refuses a
    lexeme above `2147483647` *before* each multiply instead of accumulating unbounded: the
    accumulation was signed overflow (UB, and a wrap in practice), so a raw
    `{"nodeID":18446744073709551646}` (2^64 + 30) became node **30**, passed the `id >
    NODEID_MAX` check and made `entity.get` / `entity.setKeyValues` / `node.delete` / the mesh ops
    read or **write a different node**. `2147483647` is still accepted, `2147483648` is refused
    (before, it was accepted as a value the caller then refused - now the reader refuses it), and a
    negative still parses to its sign (every nodeID/limit caller's check is `>= 0` / `1..2000`, so a
    `-1` stays the caller's own `out of range` message). `json_int`, the lenient depth-blind finder
    the other integer params go through (`expectDoc.epoch`, `entity.render`'s and
    `pet.render`'s `width`/`height`, the render `format`, `input.set`'s `frames`, `node.list`'s
    `limit`, `hello`'s `protocol`, `deadlineMs`, and `game.entities`'/`game.selectWeapon`'s
    `max`/`entindex`/`slot`/`defIndex`/`handle`), refuses only a lexeme that would overflow
    `long long`. None of those callers has a range anywhere near either bound, and most of them
    read a failure as "the key is absent": the param keeps its default (`deadlineMs` the default
    deadline, `node.list` limit 2000, width/height their defaults, `max` 64) or `entindex` leaves
    `single = 0` and the whole entity list comes back. Fail-closed are only `epoch`
    (`missing_epoch` -> `-32004 DocumentChanged`), `protocol` (stays 0 -> `-32002
    VersionMismatch`) and `selectWeapon` (falls through to its "one of slot/defIndex/handle
    required"). Either way nothing wraps: the client-side params are covered too - a raw
    `{"entindex":18446744073709551669}` used to wrap to `5`, and `selectWeapon`'s
    `slot`/`defIndex`/`handle` could wrap to another weapon's index.
13. **No non-finite number reaches a caller** (review N3). `json_num` refuses a result that is not
    finite, so `{"origin":[1e400,0,0]}` - and a 400-digit literal with no exponent, which
    accumulates to `inf` - is a malformed vector (`-32602 origin: [x,y,z] required` on
    `entity.create`) instead of an infinite origin; `json_float1_at` / `json_float3_at` additionally
    refuse a *finite* double that narrows to `+-inf` as `float` (`1e39` is above `FLT_MAX`), which
    is the type origins/angles/radii are stored in, and a refused vector is not written to the
    caller's variable at all (`fov`/`znear`/`zfar` are read with the return value ignored, so they
    must not receive the value the check just rejected). `kv_next`'s own number parser (the
    keyvalue path) is bounded the same way: an integer token past the `long long` range is refused
    instead of cast (the cast was UB and `(int)ival` then stored a different number) and a float
    token past `FLT_MAX` is refused instead of written to a float attribute as `inf`. Legit large
    literals (`1e20`, `5.5e-17`, `9e18`) parse as before. The keyvalue path is raw-protocol only:
    the Rust layer sends every keyvalue as a JSON string.
14. **`kv_next` compares `true`/`false`/`null` whole** (review L6). The old scan recognised them
    from one or two bytes (`p[0] == 'n'`) and then returned `p + 4` / `p + 5`, so a raw frame that
    ended inside a literal (`{"k":tr`, `{"k":n`, `{"k":fa`) handed the caller a pointer past the
    NUL. The comparison is now the whole literal (the region's `strncmp`, `lit_at`, which stops at
    the NUL) and a truncated literal is malformed (`kv_next` -> 0), never a value. MCP cannot reach
    this path: the Rust layer sends every keyvalue as a string.
15. **The standalone `entity.create` arm validates before it opens its undo group** (review L3,
    closing the gap FIX-B item 11 left open). `op_entity_create` is split into
    `entity_create_target` (class present, non-empty and in the node-factory registry;
    `origin`/`angles` well-formed) and `entity_create_apply` (the node creation plus the keyvalue
    write), and the arm runs the target check **before** `undo_open(doc, "MCP: create entity")` -
    the same shape as `set_keyvalues_target`. A refused raw create no longer leaves an empty
    `"MCP: create entity"` group for `undo_last` to pop. `edit.batch` keeps opening its group
    before its ops: its validation is per op and happens *mid-batch* (a later op fails after
    earlier ones ran), so there is no wholesale refusal to hoist. When a batch's **first** op is
    refused the group is empty, but hammer-mcp counts a `PartialFailure` batch as `+1`, so the
    counter still balances - and that `+1` needs a group to pop: hoisting the first op's validation
    out of the batch would leave the counter's `+1` with no group of its own (it would pop the
    previous, unrelated one). The batch group therefore stays where it is.
16. **A batch with no `entity.create` op is labelled `"MCP: set keyvalues"`** (review section 3).
    `undo_last` lowers the unsaved-edit counter for any popped `"MCP: batch"` group; a batch of
    nothing but `entity.setKeyValues` ops reverts nothing when popped, so that label was wrong for
    it (the counter dropped while the values stayed changed). The label is now decided before the
    group is opened, from a scan of the ops (`batch_has_create`, next to `batch_op_next`), and is
    `"MCP: batch"` only when the batch contains a create op. `"MCP: set keyvalues"` is exactly the
    string hammer-mcp's `KEYVALUE_UNDO_LABEL` matches (`entities_live.rs`), so `note_undo` is
    skipped, the counter stays up and the group is still "ours" for the `acceptUserEdits` check
    (`starts_with("MCP:")`). A batch with no readable ops at all also takes the keyvalue label:
    nothing was created, and that is the label that never moves the counter. The scan uses the same
    splitter as the batch run, so it describes exactly the ops that will run. **Known limit:** the
    label describes the whole ops array, while the run stops at the first refusal, so a raw mixed
    batch can still disagree - `[setKeyValues, create-that-fails]` is labelled `"MCP: batch"` and
    holds only the keyvalue write, and `[create, setKeyValues]` reverts the create but not the
    values. Only a raw-protocol client can send a mixed batch (`create_entities` sends creates
    only), and the label can be changed only while the group is being opened, so this is left as
    it is. hammer-mcp's own tool text still says an `edit.batch` group is always `"MCP: batch"`
    (`entities_live.rs` line ~99/155/962, outside this pass's file scope): a follow-up for the Rust
    side.

**Offline proof.** `./bridge/build.sh` is green: the DLL builds warning-free and the host unit test
`bridge/test_entity_json.c` runs **54 cases, 0 failures** (42 before: the new ones are
`json_int_at_bounded_to_int32`, `json_int_no_signed_overflow`, `json_num_rejects_non_finite`,
`json_float_rejects_narrowed_inf`, `json_find_top_nodeid_out_of_range_refused`,
`kv_next_literal_at_frame_end`, `kv_next_truncated_literal_is_malformed`, `kv_next_null_complete`,
`kv_next_refuses_unrepresentable_numbers` and the three `batch_label_*` cases). The
truncated-literal case runs ten frames on a guard-page buffer whose last byte is the terminator
(the next page is `PROT_NONE`), so a read past the NUL faults instead of passing silently; it also
requires the returned pointer to stay at the NUL. **Negative evidence** - one revert at a time in
`bridge/mcp_bridge.c`, the new test unchanged, `build.sh` exiting 1 each time:

- the old unbounded digit loop: `json_int_at_bounded_to_int32` fails with
  `20digit=1(30 wrapped)`, `json_int_no_signed_overflow` fails, and
  `json_find_top_nodeid_out_of_range_refused` fails - the 20-digit id really did read node 30
  (3 failures);
- the old `json_num`/float helpers: `json_num_rejects_non_finite` fails with `1e400=0(inf)
  400-digit=0(inf)` and `json_float_rejects_narrowed_inf` with `1e39_3=0(origin=inf)` (2 failures);
- storing the narrowed float before the finiteness check (the old shape):
  `json_float_rejects_narrowed_inf` fails with `1e39_3=1(origin=inf) 1e39_1=1(inf) untouched=0` -
  the caller receives `inf` in its own variable (1 failure);
- `kv_next` without the representability check: `kv_next_refuses_unrepresentable_numbers` fails
  with `20digit=0 1e400=0 1e39=0` - all three were accepted as keyvalue numbers (1 failure);
- the old one/two-byte literal scan: `kv_next_truncated_literal_is_malformed` fails with
  `10 frames: 0 unmapped, 8 not malformed` - the frames ending in `tr`/`tru`/`n`/`nu`/`nul`/`fa`/
  `fal`/`fals` were accepted as values from a pointer past their NUL (the bare `{"k":t` the review
  named, and `{"k":f`, already returned 0 on the old code) (1 failure);
- the label scan forced to the old always-`"MCP: batch"` answer: `batch_label_set_only_is_keyvalues`
  and `batch_label_reads_the_method_member` fail (2 failures).
