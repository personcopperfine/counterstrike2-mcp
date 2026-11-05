# Protocol changes in m3.4 — lane MODELDOC (`modeldoc.active`, `modeldoc.open`, `modeldoc.reload`, `modeldoc.save`, `modeldoc.compile`, `modeldoc.nodeTypes`)

Bridge build `m3.4` (Rust bridge) = m3.3 + this group.

Lane branch `bridge/rust-m4-modeldoc` on base `833c76a`; the orchestrator bumps the build at
merge (this report leaves `BRIDGE_BUILD` alone).

**One row group moved from "not ported" to served — minus one row.** The Rust bridge now
answers the six ModelDoc document methods byte-for-byte like the C bridge at m2.20.
`modeldoc.render` is the render lane's (`bridge/mcp_bridge_modeldoc_render.inc`) and stays
unregistered: it keeps the `-32011 NotImplemented {"blockedOn":"rust bridge m3.0: not ported
yet"}` answer where the C serves it. Nothing else changed: the method table is unchanged
(84 registered / 69 implemented), no new error code, no changed field.

## The wire, field by field

| Where | m2.20 (C bridge) | this build |
|---|---|---|
| `modeldoc.active` result | `{"doc":null,"reason":<hop>}` or `{"doc":{path, dirty, isNewFile, hasAsset, compileStatus:null, docType:"vmdl"}}` (`mcp_bridge.c:5201-5213`) | identical bytes; never errors on no doc |
| `modeldoc.open` result | `{"opened":b,"path":<collapsed path>,"active":<doc object or null>}` (`:5214-5272`) | identical; the seven param refusals (`:5218`, `:5222`, `:5229`, `:5235`, `:5240`, `:5244`, `:5253`) run in the C's order before any game access |
| `modeldoc.reload` result | `{"reloaded":b,"path":<captured path>,"discardedChanges":b}` (`:5273-5305`) | identical; `-32016 UnsavedChanges` without `force`, `-32015 UntitledDocument` for `isNewFile`/empty path |
| `modeldoc.save` result | `{"saved":b,"skippedClean":b,"path":<copied path>}` (`:5306-5333`) | identical; a clean document skips the API call; `SetModified(0)` only after a successful `SaveAndNotify(path, path, 0, 0)` |
| `modeldoc.compile` result | `{"status":null,"failed":null,"savedFirst":b}` (`:5334-5351`) | identical; `-32017 PrerequisiteFailed {"reason":"no_asset"}` without an asset; the compile fn is the pinned `MDFN_SESSION_COMPILE`, called with mode 0 |
| `modeldoc.nodeTypes` result | `{"count":N,"types":[{"typeId":N,"className":"…"}]}` (`:5352-5387`) | identical; the count bounds (1..4096), the unreadable-table / unreadable-descriptor / unreadable-className `-32017` reasons, the 128-byte bounded className copy |
| error codes | `-32003 NoActiveDocument`, `-32015 UntitledDocument`, `-32016 UnsavedChanges`, `-32017 PrerequisiteFailed`, `-32602 InvalidParams` | identical, same `reason` strings, same `{"reason":…}` `data` objects |
| result buffer / overflow | 256 KiB `rbuf`, `-32603 "InternalError: modeldoc result too large"` (`:6206`, `:6212`) | identical (`finish` over the same 256 KiB `Sb`) |

## What the bodies share

* `resolve_modeldoc` (`:5124-5145`): `mod_ok(PIN_MODELDOC)` → module-handle equality →
  `CreateInterface("ToolSystem2_ModelDoc")` (**ret 0 = found**, the code logged on failure) →
  the static-object equality (`MDG_APP_OBJ`) → `obj_is(MDV_APP_VT)`; resolved once into the
  process-wide app static (main thread only, like the C's `g_modeldoc`).
* `active_modeldoc` (`:5147-5167`): the same per-hop reason chain
  (`modeldoc_mismatch` / `modeldoc_verifying` / `modeldoc_not_loaded` / `modeldoc_unavailable` /
  `no_session` / `bad_session` / `no_document` / `bad_doc`), and `session->doc`
  identity-checked against `MDV_DOC_VT` per call (the doc is replaced on every load/reload).
* `md_doc_json` (`:5182-5194`): the shared doc object; `compileStatus` is always `null` (the
  embedded `CModelDocAnalysis` is an IErrorListener, never read).
* `md_str` (`:5169-5180`): the page-boundary-probing bounded copy, ported in this module (the
  pet lane ports its own; the orchestrator dedups at merge). It is deliberately not
  `ops::hammer`'s `probe_str`: the C's `md_str` has neither the exact-fit check nor the
  UTF-8-boundary cut, so a cut string keeps its raw cap-1 prefix here.

## Places the port deliberately differs from the C (no byte on the wire)

1. **Unguarded game reads got `mem_ok` guards** where the C would fault on a corrupt pointer:
   each `descs[i]` slot word of the node-type array (`:5368` reads it after proving only the
   array's first and last slot) and the compile-fn unbound case (`g_mdCompile` is non-null
   whenever the C reaches the call because `mod_ok` was checked first; the Rust `panic!`s and
   the drain answers `-32603` instead of jumping to null). Readable inputs produce the same
   bytes.
2. **The bool-returning session slots are called through the C header's `u8` signatures.**
   The generated `CBaseEditorSession_vtable::IsModified` / `::Save` return a Rust `bool`,
   which the wave-1 rule forbids forming from a game return; the slots are called through
   `CBaseEditorSession_slots::{IsModified, Save}` with the C header's own signatures
   (`bridge/s2sdk.h:567`, `:573`, both `u8`). The generated typed fields are used for
   `ReloadDocument` (slot 7), `SetModified` (slot 12), `GetPath` (slot 13) and `OpenFile`
   (app slot 18) - all pointer/void shapes, no game-side `bool` crosses.
3. **`resolve_modeldoc` calls the factory export directly** (like `ops::hammer`'s
   `resolve_hammer`) instead of `s2sdk::module::Module::create_interface`, because the C's
   failure branch logs the factory's `ret` value (`:5134-5135`) and the safe wrapper does not
   surface it. The module-handle check compares `GetModuleHandleW` with the verified base
   (`Module` exposes no base accessor), as in the HAMMER lane.
4. **Non-Windows builds have no reader.** `modeldoc_editor.dll` is a Windows module and the
   generated modeldoc bindings are `cfg(windows)`; on Linux the module, its builders, its
   validation and its registration still exist (the gate answers `-32017 modeldoc_editor.dll`
   exactly as it would on Windows with the module missing), `modeldoc.active` answers the C's
   no-module shape (`{"doc":null,"reason":"modeldoc_not_loaded"}`) and the reload/save/compile
   arms the C's `-32003 NoActiveDocument` — the C's own behaviour for a machine without the
   module. Any deeper read fails loudly instead of inventing an answer. The live parity pass
   runs the Windows build, which is the one that matters.
5. **`modeldoc.render` stays `-32011 NotImplemented`** until the render lane ports it (the C
   serves it). Every other name the C's modeldoc arm reaches is served.

## Python parity coverage (`bridge/test_modeldoc.py`)

The script's live cases cover this group: the `modeldoc.active` doc shape and
`compileStatus:null`, `modeldoc.nodeTypes` (`count > 0`, classNames), `modeldoc.reload`
(`reloaded:true`, `discardedChanges:false`), `modeldoc.open` (the opened file's path in
`active.path`), and the `-32017 no_asset` refusal for an asset-less document
(`test_modeldoc.py:254`, `:268`, `:295`, `:300-303`). Its `--static` mode (offline) greps the
**C** source (`bridge/mcp_bridge.c`) for the m2.14 source contract — `BRIDGE_BUILD` define,
method list, render tail (`test_modeldoc.py:114-149`); to check the Rust instead it would
need to read `crates/cs2-bridge/src/ops/modeldoc.rs` + `src/ops/mod.rs` and
`crates/cs2-bridge/src/registry.rs`. The in-crate tests hold the Rust to that contract today:
`ops::modeldoc::tests::*` (the byte-exact builders, the reason chain, the param order,
the 256 KiB overflow) and `tests/ops_modeldoc.rs` (the `-32017` gate naming
`modeldoc_editor.dll`, the capabilities advertisement, the wire shapes over
`wiring::BridgeState`).
