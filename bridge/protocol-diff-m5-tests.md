# The live parity suites, retargeted at the Rust bridge (M5)

The C bridge (`bridge/mcp_bridge.c` + `bridge/*.inc`, frozen at build `m2.20`) is being
replaced by the Rust bridge (`crates/cs2-bridge`, `BRIDGE_BUILD` in `crates/cs2-bridge/src/lib.rs`,
`m3.6` at the time of this pass). This pass moves the live parity scripts off the C text:

* **Every version check** in `bridge/test_*.py` compares against `bridge_build.expected_build()`
  instead of the literal `"m2.20"` (step names unchanged). The helper reads the single
  `pub const BRIDGE_BUILD: &str = "m…";` in `crates/cs2-bridge/src/lib.rs` and raises if there is
  not exactly one.
* **The five `--static` modes** (`test_entities`, `test_triggers`, `test_sound_params`,
  `test_modeldoc`, `test_props`) read the Rust sources: the `METHODS` table in
  `crates/cs2-bridge/src/registry.rs`, the `HANDLERS` tables under `crates/cs2-bridge/src/ops/`,
  and the generated pins in `crates/cs2-bridge/src/generated/pins.rs`. They keep only checks
  that still have a source fact to make, and they **fail on a missing/renamed file** (the helpers
  raise; each static mode prints `FAIL: cannot read the Rust bridge sources: …` and exits 1).
* **Everything else** - the C function bodies, dispatch arms, refusal ordering inside the C, the
  C headers' `_Static_assert`s, `protocol-diff-m2.*.md` contents and the
  `git show <ref>:bridge/mcp_bridge.c` diffs - is retired with the C sources. The tables below
  name each retired check, why it cannot survive the cutover, and the Rust unit test that covers
  the same contract where one exists.

Shared helper: `bridge/bridge_build.py` (`expected_build`, `methods`, `handler_names`,
`pin_names`, `row_problems`, `BUILD_SHAPE`). `python3 -I` drops the script directory from
`sys.path`, so each script inserts `os.path.dirname(os.path.abspath(__file__))` and imports it
the way it already imports `test_client`.

The build-string check is now the same in all five scripts: `BUILD_SHAPE.match(build)` where
`BUILD_SHAPE = ^m(?:[3-9]|[1-9][0-9]+)\.[0-9]+$` - the Rust bridge's `m3.N` or later, never the
C's `m2.x`.

## test_entities.py

**What changed.** `BUILD = expected_build()` (live `version` step unchanged). The static mode
shrank to the registry rows this pass owns and the pins behind them:

| kept | checks |
|---|---|
| `static_build_string` | the single `BRIDGE_BUILD`, `m3.N` or later |
| `static_rows_implemented` | `entity.get`, `entity.find` in `METHODS`, `implemented: true`, `hidden: false`, `blocked_on: None`, needs exactly `NEED_HAMMER` |
| `static_rows_served` | both names registered in `ops/hammer.rs`'s `HANDLERS` |
| `static_undo_redo` | `undo.redo` row on `NEED_HAMMER`, served |
| `static_props_pass` | the three prop rows (`node.setTransform`, `entity.addConnections`, `entity.removeConnections`) implemented + served, and `HFN_GET_REDO_HISTORY`, `HFN_SET_KEY_VALUE`, `HFN_ADD_CONNECTION`, `HFN_REMOVE_CONNECTION`, `HFN_KV3_INIT`, `HFN_KV3_FREE` present in `generated/pins.rs` |

Retired static checks:

| retired check | why | Rust test covering the same contract |
|---|---|---|
| `static_readonly_dispatch` (`geo_dispatch(..., mutating=0)` arms) | a C function-body/dispatch-arm check; the dispatch chain is `crates/cs2-bridge/src/ops/mod.rs` + `dispatch.rs` now | `handlers_are_table_rows_and_are_ported` (`tests/ops_game.rs`), `every_handler_names_a_c_row_this_group_implements` (`ops/hammer.rs`) |
| `static_nodeid_branch` (the C `handle`-or-`nodeID` branch and its four refusals) | the C text is retired; the same gates live in the Rust body | `entity_get_param_errors_are_the_c_codes_and_messages`, `set_keyvalues_param_errors_are_the_c_codes_and_messages` (`ops/hammer.rs`) |
| `static_nodeid_range_checked` (`NODEID_MAX`, `json_int_at`, the 2147483647 reason) | C parser text | the same two param-error tests (they pin the codes/messages) |
| `static_top_level_lookup` (`json_find_top`/`json_*_top` at every param site) | C scanner text; the Rust params live in `Request`/`entity_json` | `params_span_is_balanced_and_string_aware` (`ipc/envelope.rs`) |
| `static_batch_splitter_string_aware` (`json_bal_end`, no brace counting) | C batch splitter; `edit.batch` is still a C-only method and its body retires at cutover | `params_span_is_balanced_and_string_aware` (`ipc/envelope.rs`) |
| `static_no_silent_truncation` (`KV_VALUE_MAX`, `previousTruncated`, `truncatedValues`) | C buffers/field markers | `the_keyvalue_lists_overflow_at_the_c_buffer_sizes`, `entity_get_name_lists_overflow_their_1024_byte_builders` (`ops/hammer.rs`) |
| `static_kv_next_unicode_decoding` (`json_estr`, surrogate constants, `\b`/`\f`/`\/`) | C scanner text | `c_test_cases_ported_one_for_one` / `differential_corpus_matches_the_c` (`tests/entity_json.rs`) |
| `static_utf8_safe_frames` (`utf8_seq`, `utf8_prefix`, U+FFFD) | C scanner text | `invalid_utf8_game_text_becomes_one_replacement_character_as_the_c` (`ops/hammer.rs`) |
| `static_validate_before_undo` (`set_keyvalues_target` before `undo_open`) | C ordering inside the arm | `set_keyvalues_applies_through_the_c_paths` (`ops/hammer.rs`: the refused call opens no group) |
| `static_create_ovf_checked` (the C 16384-byte result buffer/overflow text) | C buffer sizes | `finish_builds_the_c_arm_tails` (`ops/mod.rs`), `the_keyvalue_result_is_the_c_byte_sequence` (`ops/hammer.rs`) |
| `static_string_coercion` (SetKeyValue write, no `CreateAttribute(..., kind)`, warnings array) | C body | `set_keyvalues_applies_through_the_c_paths` (`ops/hammer.rs`) |
| `static_previous_and_valueundo` (`previous`, `previousTruncated`, `valueUndo:"recorded"`) | C result text; the Rust builders own the bytes | `set_keyvalues_applies_through_the_c_paths`, `undo_replies_carry_the_c_result_shapes` (`ops/hammer.rs`) |
| `static_batch_whitelist` (`edit.batch`'s `entity.create`/`entity.setKeyValues` arms) | C body; `edit.batch` is not ported yet | none yet - `edit.batch` is one of the rows the Rust bridge still does not serve |
| `static_no_new_hex_literals` (`git show main:bridge/mcp_bridge.c` diff) | there is no C file to diff; the C generator is frozen and pins are generated | `pins.py`/`sdkcheck.py` own the no-hand-written-number rule; `has_hex_literal` in `tests/entity_json.rs` guards the C region while it exists |
| `static_write_doc_tail` (`sb_add_doc_object`/`sb_add_doc_tail` and every call site) | C writers | `the_doc_tail_splices_before_the_closing_brace` (`ops/hammer.rs`) |
| `static_undo_redo`'s pin/prototype/arm checks (`HFN_GET_REDO_HISTORY` macro, `s2sdk.h` prototype, `GetRedoHistory` call, `redoDepth` text) | C header/body; the row check stays | `undo_steps_are_the_c_gates_and_pop` (`ops/hammer.rs`) |
| `static_props_pass`'s op/undo-group/order/writer/s2sdk.h checks (the `op_*` functions, `undo_open` labels, slot 19/18/17 order, `conn_list_json`, `RemoveConnection`, the `geo_reset_mesh` slot-18 fix and the `S2ConnectionSrc`/`CMapNode` asserts) | C body/header | `node_set_transform_writes_scales_angles_origin_in_that_order`, `add_connections_runs_the_c_recipe_and_reads_back`, `remove_connections_removes_descending_and_reports_ascending`, `entity_get_builder_is_the_c_byte_sequence` (`ops/hammer.rs`) |
| `static_protocol_diff` (`protocol-diff-m2.15.md` contents) | the file documents the C change and stays as history; the Rust change is `protocol-diff-m3.*.md` | n/a |
| `static_host_unit_test_wired` (`bridge/test_entity_json.c` + `build.sh`) | the C host test and its build wiring retire with the C region; the Rust parsers carry their own tests | `tests/entity_json.rs` (`c_region_test_passes`, `differential_corpus_matches_the_c`) while the C region exists |

The `--base-ref` option is kept for CLI compatibility and ignored.

## test_triggers.py

**What changed.** `BUILD = expected_build()`. The single static step now checks the Rust rows and
pins only:

| kept | checks |
|---|---|
| `static_build_string` | the single `BRIDGE_BUILD`, `m3.N` or later |
| `static_triggers_contract` | `entity.tieMeshes` / `entity.untie` in `METHODS` (`NEED_HAMMER`, implemented, advertised, not blocked); every `HANDLERS` name is an implemented row; `HFN_DOC_MOVE_TO_WORLD` / `HFN_RECORD_NODES_AND_PARENTS` in `generated/pins.rs` |

Both trigger bodies are still other lanes' work: the script prints
`NOTE: no Rust body yet for [...]` instead of silently passing the handler half. Once a lane
registers the handler, the same step covers it (the `HANDLERS` invariant is global).

Retired static checks (the old single step's needles, grouped):

| retired check | why | Rust test covering the same contract |
|---|---|---|
| the two `geo_dispatch(..., mutating=1)` dispatch arms | C dispatch arms | none yet - the trigger methods are not ported |
| tie: `undo_open`, `RecordNodesAndParents`, `AddChild`, `CreateNodeByName`, first-mesh origin/angles, `gd_type`, point-class refusal, delete, `apply_keyvalues`, `"already tied"`, emptied-entity delete, `DocSetModified`, `removedEmptyEntities`/`alreadyTied`/undo-label reply fields | C op body | none yet (unported). The row itself is held by `table_matches_the_c_row_for_row` (`tests/registry.rs`) |
| tie: `UnknownClass`, `tie_err_undo_group` markers, `tie_kv_prescan`, `tie_ancestors_ok`, parent lock, the all-alreadyTied no-op reply, the external `NodeVec` marker | C op body | none yet |
| tie: the validation-before-`undo_open` order (`gate < undo_open < RecordNodesAndParents < AddChild`), every refusal before the group, the in-group solid-class exception | C text order | none yet |
| untie: `undo_open` absent, `DocMoveToWorld`, brush gate, `Untie from entity`, `entityDeleted`, `NodeVec`, `meshesTruncated`, `"entity has no parent"` | C op body | none yet |
| `entity.get` children writer (`entity_children_json`, `CHILDREN_LIST_MAX`, `childrenTruncated`, the `children` field, null nodeID) | C writer; the Rust `entity.get` body exists | `a_negative_child_node_id_prints_null_like_the_c` (`ops/hammer.rs`) |
| pins `HFN_DOC_MOVE_TO_WORLD` / `HFN_RECORD_NODES_AND_PARENTS` in `bridge_pins.h` | the header is retired | kept, retargeted to `generated/pins.rs` |
| `S2_OFF_FgdClass_t_m_nType` in `s2sdk_gen.h` | the C generated header retires with the bridge; the offset is the Rust SDK's generated layout | the s2sdk-sys generated layout tests (the same header feeds `gen.py`) |
| `s2sdk.h`: AddChild slot/prototype, `CMapNode` layout, `RecordNodesAndParents`/`DocMoveToWorld` prototypes | C header asserts | the s2sdk-sys vtable/layout tests (`gen.py --check`, `layout.rs`) |
| `protocol-diff-m2.19.md` contents | the file documents the C change and stays as history | n/a |
| the no-new-hex-literal `git show <ref>:bridge/mcp_bridge.c` diff | no C file to diff | n/a |

`--base-ref` is kept and ignored.

## test_sound_params.py

**What changed.** `BUILD = expected_build()`. The static mode now checks the build string,
`sound.drain`'s row (`NEED_SOUND`, implemented, advertised, not blocked) and the
`SNVS_SOS_START_WITH_PARAMS` pin name in `generated/pins.rs`. `--src` is retired: it drove the C
text checks, so passing it now prints an explicit `FAIL: --src is retired` and exits 1 (a static
mode never skips silently).

Retired static checks:

| retired check | why | Rust test covering the same contract |
|---|---|---|
| `BRIDGE_BUILD "m2.20"` in `mcp_bridge.c` | replaced by `expected_build()` | - |
| `S2_OFF_SosStartSoundEventParams_t_m_pFieldData` = 8 / `…_m_nFieldDataSize` = 28 in `s2sdk_gen.h` | the C generated header retires; the offsets are the Rust SDK's generated layout | the s2sdk-sys layout tests (`gen.py --check`) |
| `S2_SLOT_ISoundOpSystem_StartSoundEvent_2` = 11 in `s2sdk_gen.h` | same | s2sdk-sys generated slot table |
| the `SNOFF_SEP_PACKED_PTR`/`SNOFF_SEP_PACKED_LEN` sigs entries and their locator pattern | the capture lives in the C include; `sound.*` is not ported yet, so no Rust text reads these pins | none yet (sound lane unported); `pins.py` keeps the sigs entries validated |
| `SNVS_SOS_START_WITH_PARAMS` in `bridge_pins.h` | the header is retired | kept, retargeted to `generated/pins.rs` |
| `SndEvt` gained `plen`/`pcopied`/`praw` | C ring-entry layout | none yet |
| `snd_capture_packed`: the pin reads, the 0..65535 clamp, `plen = -1`, the per-page `mem_ok` probe, and the "no literal `0x1C` / `+ 8`" negative | C helper text | none yet |
| exactly one `snd_capture_packed` call site; the hook's init < capture < push order and params-struct guard | C body | none yet |
| the `op_sound` drain's slot-11 guard, the `params` writer inside it and its `len`/`hex`/`truncated` fields, the writer appearing exactly once | C body | none yet |
| the `dropped`/`total`/`pending` tail | C body | none yet |
| `protocol-diff-m2.20.md` contents | the file documents the C change and stays as history | n/a |
| the no-new-hex `git show <ref>:bridge/mcp_bridge.c` diff | no C file to diff | n/a |

## test_modeldoc.py

**What changed.** The static mode now checks the Rust registry and pins:

| kept | checks |
|---|---|
| `static_build_string` | the single `BRIDGE_BUILD`, `m3.N` or later |
| `static_method_list` | all seven `modeldoc.*` names in `METHODS` |
| `static_method_rows` | each implemented on `NEED_MODELDOC`, advertised, not blocked |
| `static_pet_render_row` | `pet.render` row on `NEED_PET`, implemented, advertised, not blocked |
| `static_modeldoc_served` | the six bodies that have landed (`active`, `open`, `reload`, `save`, `compile`, `nodeTypes`) are in `ops/modeldoc.rs`'s `HANDLERS` |
| `static_modeldoc_pins` | `MDOFF_MAINWND_SCENEWIDGET`, `MDOFF_SCENEWIDGET_TOOLWIDGET`, `MDOFF_TOOLWIDGET_SCENE`, `MDOFF_TOOLWIDGET_CAMERA`, `MDV_SESSION_VT`, `MDV_EDITORWND_VT`, `MDV_SCENEWIDGET_VT`, `MDV_TOOLWIDGET_VT`, `MDV_TOOLSCENE_VT`, `MDV_SCENECAMERA_VT`, `MDFN_TOOLSCENE_RENDER`, `PIN_MODELDOC` in `generated/pins.rs` |

`modeldoc.render` is deliberately excluded from the served set: its body is the render lane's
(`ops/modeldoc.rs` keeps it unregistered), but its row is still checked. The old
`S2_OFF_CModelDocEditorSession_m_pMainWnd` element was dropped from the pin list: it is a header
offset owned by `s2sdk-sys`, not a generated bridge pin.

Retired static checks:

| retired check | why | Rust test covering the same contract |
|---|---|---|
| `static_render_include` (`mcp_bridge_modeldoc_render.inc` included, dispatcher routes to `op_modeldoc_render`) | C include/dispatch; the Rust render lane will register a `HANDLERS` entry | none yet (render unported) |
| `static_no_pet_pins` (no `PETV_`/`PETOFF_`/`PETFN_` names in the render include) | C include text | n/a (the Rust render lane reads generated pins by name; `PIN_MODELDOC` is checked) |
| `static_chain_reasons` (the exact `modeldoc.render` chain reason set) | C chain body | `active_modeldoc_walks_the_c_reason_chain`, `active_doc_reports_each_hop_reason_in_order` (`ops/modeldoc.rs`) cover the `modeldoc.active` chain; `modeldoc.render`'s own reason chain is unported |
| `static_shared_tail` (`toolscene_render_png`/`toolscene_render_params` defined once, used by both ops) | C shared-helper text | none yet (render unported) |
| `static_result_shape` (`RENDER_RESULT_SKELETON` out-literals) | C JSON builder text | none yet (render unported); `md_doc_json_is_byte_exact` and `node_types_rows_are_byte_exact` cover the ported modeldoc replies |

## test_props.py

**What changed.** The single static step now checks the three rows, their handler entries and
the five pins:

| kept | checks |
|---|---|
| `static_build_string` | the single `BRIDGE_BUILD`, `m3.N` or later (inside `static_props_contract`) |
| `static_props_contract` | `node.setTransform`, `entity.addConnections`, `entity.removeConnections` in `METHODS` (`NEED_HAMMER`, implemented, advertised, not blocked) and in `ops/hammer.rs`'s `HANDLERS`; `HFN_SET_KEY_VALUE`, `HFN_ADD_CONNECTION`, `HFN_REMOVE_CONNECTION`, `HFN_KV3_INIT`, `HFN_KV3_FREE` in `generated/pins.rs` |

Retired static checks:

| retired check | why | Rust test covering the same contract |
|---|---|---|
| the three `op_*` functions and their `geo_dispatch(..., mutating=1)` arms | C body/dispatch | `node_set_transform_writes_scales_angles_origin_in_that_order`, `add_connections_runs_the_c_recipe_and_reads_back`, `remove_connections_removes_descending_and_reports_ascending` (`ops/hammer.rs`) |
| the three `undo_open` group labels | C body | the same three tests |
| the SetKeyValue write, create's scale write, the transform's scale/angle/origin writes and their order | C body | `node_set_transform_writes_scales_angles_origin_in_that_order`, `set_keyvalues_applies_through_the_c_paths` |
| `Kv3Init`/`AddConnection`/`Kv3Free`/`RemoveConnection`, `rm_sort_desc` | C body | `add_connections_runs_the_c_recipe_and_reads_back`, `remove_connections_removes_descending_and_reports_ascending` |
| `conn_list_json`, the `connections` field, `-32026 ConnectionNotFound`, `op_err_maybe` | C writer/errors | `entity_get_builder_is_the_c_byte_sequence`, `a_refused_removal_is_a_maybe_executed_error` |
| the add cap refusal, read cap report, `connectionsTruncated`, match whole-value rule, add read-back resync, mesh scale reset | C body | `add_connections_refuses_to_cross_the_read_back_cap_before_the_group`, `add_connections_runs_the_c_recipe_and_reads_back`, `remove_connections_param_errors_are_the_c_codes_and_messages` |
| the remove reply's `remaining`, the add reply's `landed`, `xform_origin_reason`, `entity.create`'s nodeID, the batch recorded-node check | C body | `remove_connections_reply_is_the_c_byte_sequence`, `add_connections_reply_is_the_c_byte_sequence` |
| the `geo_reset_mesh` slot-18 fix (`SetAngles`, never `SetOrigin`) | C bug fix in a C function | none direct; the Rust `node.setTransform` order test pins the same slot mapping |
| `s2sdk.h`: the three `S2_SLOT` asserts, `CMapNode` layout, the 80-byte `S2ConnectionSrc`, the five function prototypes | C header asserts | the s2sdk-sys vtable/layout tests |
| `protocol-diff-m2.18.md` contents | the file documents the C change and stays as history | n/a |
| `test_entity_json.c` `xform_cases`/`conn_cases`/`rm_cases` and the `build.sh` wiring | the C host test retires with the C region | the Rust param tests above; `tests/entity_json.rs` while the C region exists |

## How to run

```sh
python3 -I bridge/test_entities.py --static
python3 -I bridge/test_triggers.py --static
python3 -I bridge/test_sound_params.py --static
python3 -I bridge/test_modeldoc.py --static
python3 -I bridge/test_props.py --static
python3 -I -m py_compile bridge/test_*.py
```

No harness change was needed outside `bridge/`: the test scripts, `bridge/bridge_build.py` and
this file are the only files this pass touches.
