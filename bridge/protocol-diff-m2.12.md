# Proposed protocol changes for bridge m2.12 (particle editor first pass: .vpcf lifecycle + registry)

Additive over m2.11; no major bump. One new pin: `PIN_PET` = `pet.dll` (late-loaded: it only
appears after `LoadToolSystem` maps it; `modules_poll` picks it up within ~32 frames,
`verify_all_thread` verifies it last - the 9.3 MB hash takes a few seconds). Seven new methods,
all executed on the tools main thread like every other game call. RE:
`hammer-mcp/research/pet-re.md`; signature macros: `sigs/signatures/pet.json` (23 entries) plus
`TFVS_LOAD_TOOL_SYSTEM` (slot 24) from `toolframework2.json`; shared contracts: the work-package
plan §0.2/§0.3 (`target/BRIEF.md`). Consumers: `hammer-mcp/src/tools/particle_live.rs` (WP5)
behind capability gating; nothing in Rust ever sees an address.

**Modality warning (test-side guard, same policy as m2.11):** `OpenFile` pops a modal for a
missing file, a path outside the content dir, and load errors - a modal blocks the main thread
while it is up (queued requests still time out normally via the IPC `QueueTimeout` sweep). Only
host-validated paths of files under `content/csgo_addons/<addon>/` are sent from a tool, written
**relative to that addon content root** (`particles/x/y.vpcf`). `ReloadDocument` never prompts
[V static], but it does **not** fail gracefully on a file pet cannot parse: live 2026-10-13 it
recursed until the Wine main thread's stack overflowed
(`err:virtual:virtual_setup_exception stack overflow`), the main thread never returned and the
whole game process wedged (even SIGTERM did not stop it; the full live table is in §7.1). The old
"a failed load leaves the document with no root" claim is **wrong** for malformed input. The bridge
therefore checks the file itself (`bridge/kv3_check.h`) and refuses with `-32602 UnloadableFile`
BEFORE the call - both `pet.reload` (the path from `GetPath`) and `pet.open` (the path resolved
from the app root + `-addon`) run the same gate (G1/G2). Writing a broken file to disk while pet
has it open is harmless (no file watcher); only pet's load wedges. `LoadToolSystem` pops a modal on
failure and the framework caches a failed load forever: `pet.load` calls it **at most once per
session** (a bridge-side latch memoizes the one attempt's answer, so a second `pet.load` never
reaches the framework again) and reports `loaded:false` as a result - restart the runtime, do not
retry.

## §7 method table — new rows

Every row is `[I] until live`: the object chain is [V] in the IDB and the call paths are [V]
static, but no method has been observed answering against a running pet yet (the first live run
happens after WP1 is built and installed; the protocol doc's `[V runtime]` tags are added then).

| Method | Status | Bridge m2.12 | Params | Result | Call (page) | Notes |
|---|---|---|---|---|---|---|
| `pet.load` | implemented `[I] until live` | gate `NEED_TF2` (**not** `NEED_PET`: pet.dll does not exist yet) | `{}` | `{loaded:bool, alreadyLoaded:bool, toolIndex:int\|null, module:"not_loaded"\|"verifying"\|"verified"\|"mismatch"}` | `mod_iface(PIN_TF2, S2_IFACE_TOOLFRAMEWORK2)` -> `obj_is(TFV_FRAMEWORK_VT)`; `GetToolIndexByName(fw, &idx, "pet")` (slot 22, 1-byte out, `0xFF` = unknown); `LoadToolSystem(fw, (u8)idx)` (slot `TFVS_LOAD_TOOL_SYSTEM` = 24) | `-32017 {reason:"framework_unavailable"}` when toolframework2's `ToolFramework2_002` is not the pinned `CToolFramework2`; `-32017 {reason:"tool_index_unknown"}` on `0xFF` (LoadToolSystem is then never called: a failed load is cached forever and pops a modal). When `GetModuleHandleA("pet.dll")` is already non-null nothing is called: `{loaded:true, alreadyLoaded:true, toolIndex:null, module:<state>}`. Otherwise `loaded = (LoadToolSystem rc != 0 && pet.dll is now mapped)`; `toolIndex` is the confirmed index. **`loaded:false` is a result, not an error** - a modal may be up and the session has to be restarted; the bridge-side latch answers a second `pet.load` from the memoized result, so nothing can re-pop the modal. `module` is the module state for pet.dll *after* the call (`not_loaded`/`verifying`; the poll picks the DLL up right after). |
| `pet.active` | implemented `[I] until live` | `NEED_PET` | `{}` | `{doc:{path, dirty:bool, isNewFile:bool, hasRoot:bool, arrays:[{type:int, count:int, classes:[string]}]\|null, docType:"vpcf"}}` or `{doc:null, reason}` | pet.dll's exported `CreateInterface("ToolSystem2_001")` (ret 0 = found) identity == `PETG_APP_OBJ`, vtable `PETV_APP_VT`; `app+PETOFF_APP_SESSION` -> `PETV_SESSION_VT`; `session+PETOFF_SESSION_DOC` -> `PETV_DOC_VT`; `doc+PETOFF_DOC_ROOT` -> `PETV_ROOTNODE_VT`; per type 0..8 `PETFN_ROOT_GETCHILDCOUNT(root, type)` then `PETFN_ROOT_GETCHILD(root, type, i)` + node vtable byte offset `PETOFF_NODE_CLASSNAME`; `PETVS_SESSION_ISDIRTY` (11), `PETVS_SESSION_GETPATH` (13), `u8 session+PETOFF_SESSION_ISNEW` | read-only, main thread. **Never errors on no document.** `reason` is one of `pet_not_loaded`, `pet_verifying`, `pet_mismatch`, `pet_unavailable`, `no_session`, `bad_session`, `no_document`, `bad_doc`, `bad_root` (the first four are defensive: the `NEED_PET` gate answers `-32017` while pet.dll is unverified). **`doc+PETOFF_DOC_ROOT == 0` is the "no root" state (an empty/untitled document; a malformed file never gets that far - see `pet.reload`), not an error:** `hasRoot:false`, `arrays:null` (the same object is still reported, so a caller can see the empty document). A non-null root that is not the pinned `CPetRootNode` reports `{doc:null, reason:"bad_root"}`. `arrays` covers all nine type indices 0..8 (the whole table §0.1); a count outside `0..4096`, or an unreadable child/class name, fails the call closed with `-32017 {reason:"root_child_count_unusable"}` / `{reason:"root_child_unreadable"}` - never a partial list. `path` is an absolute Windows path from `GetPath` (`""` when unset). |
| `pet.open` | implemented `[I] until live` | `NEED_PET` | `{path}` addon-content-relative `particles/x/y.vpcf`, 1..512 chars, no `..` | `{opened:bool, path, active:{...piece as pet.active doc}\|null}` | the G2 gate (resolve the host file, read it, `vpcf_check` it), then `app->OpenFile(app, path, 1)` (app slot `PETVS_APP_OPENFILE` = 18, returns the session or `0`) | `-32602` on a bad path, refused **before** `OpenFile` (its modals block the main thread): JSON escapes, control characters, absolute or drive-qualified paths, a non-`.vpcf` extension, `..`. `-32003 NoActiveDocument {reason:"pet_unavailable"}` when the app object does not resolve. `opened:false` when `OpenFile` returned `0` (failure / re-entrancy guard `app+0x1A3`) - a **result**, not an error, because the call may also just re-activate an already-open file. `active` is re-resolved after the call (the opened session when it succeeded, else the prior one); an unusable root there reports `active:null` (no reason key), while an unusable **array** fails the whole call closed. Relative paths resolve in the `CONTENT` then `GAME` search paths, which are addon-rooted [I: `pid`/`GetPath` root unchecked live]. May pop a modal for a missing/broken file - the tool pre-checks; say so in any report. **G2: the bridge gates the file here too.** pet resolves the addon-relative path itself, so the bridge derives the same file on the host - `<app>\content\csgo_addons\<addon>\<path with `/` -> `\`>` - where `<app>` is the directory 4 components above `GetModuleFileNameA(NULL)` (the exe is `<app>\game\bin\win64\cs2.exe`, e.g. `Z:\app\...` under Wine: the last 4 components must be `game`/`bin`/`win64`/`cs2.exe` case-insensitively and both separators are accepted, else `-32602 UnloadableFile {reason:"no_app_root"}`; a drive root is fine) and `<addon>` is the `-addon <name>` argument of `GetCommandLineA` - `-addon mcp_test` (runtime.sh's form), `-addon=mcp_test` or `-addon "mcp_test"`, `[A-Za-z0-9_]` only, else `{reason:"no_addon"}`. Both halves are pure string code in `kv3_check.h` (`kv3_app_root`/`kv3_addon_name`/`kv3_host_path`) and are covered by the host unit test, so a broken `-addon` parse fails the build instead of failing live. The file is then read (CreateFileA/ReadFile, cap 16 MB) and must pass `vpcf_check` (`bridge/kv3_check.h`: kv3 header with format:vpcf + the KV3-text grammar + the root `_class`) - an unreadable or missing file is `{reason:"read_failed"}` (it would pop a modal anyway). `data.path` is the host file that was read, and the caller's addon-relative path for `no_app_root`/`no_addon` (no host file exists). The gate runs after the path-shape checks and before `resolve_pet`, so nothing is called when it refuses, and a malformed file is refused exactly as `pet.reload` refuses it instead of wedging pet. |
| `pet.reload` | implemented `[I] until live` | `NEED_PET` | `{force?:bool}` | `{reloaded:bool, hasRoot:bool, path, discardedChanges:bool}` | `session->ReloadDocument(session)` (session slot `PETVS_SESSION_RELOAD` = 7), **guarded**: the captured `GetPath` is read (CreateFileA/ReadFile, cap 16 MB) and checked by `bridge/kv3_check.h`'s `vpcf_check` first - kv3 header with format:vpcf + the KV3-text grammar + the ROOT object's own `_class` | `-32003 NoActiveDocument {reason}`; `-32015 UntitledDocument` for an untitled/path-less document (ReloadDocument would load `""`); `-32016 UnsavedChanges` when `IsModified` and not `force` (checked **before** the path check, the same order as `modeldoc.reload`); **`-32602 UnloadableFile` with `data:{reason, path}` when the file is not a loadable .vpcf** - `reason` is one of `binary` (a NUL byte), `no_kv3_header`, `not_vpcf`, `unterminated_comment`, `unterminated_string`, `no_root_object`, `unbalanced`, `too_deep` (nesting > 256), `trailing_content`, `expected_equals` (a key without `=`), `expected_value` (a key with no value, or a value that cannot start one), `bad_key`, `bad_token`, `comma_in_object` (objects separate members by whitespace, not commas), `expected_comma` (array elements are comma-separated), `no_root_class` and `wrong_root_class` (the ROOT object's own `_class` must be a bare string, exactly `CParticleSystemDefinition`: a nested `_class`, a `"""..."""` raw string or a flagged value does not count, and EVERY root `_class` member counts - the member name is matched case-insensitively because upstream hashes it with MurmurHash2LowerCase (`CUtlStringToken`/`CKV3MemberName`), so `_CLASS = "CWrong"` cannot slip past as a second member. Flags follow the same shape: `name:value` with the value starting immediately after the colon, the value required, no chaining (`a:b:"x"`) and no literal as the flag name), `too_large` (> 16 MB), `read_failed` (the file cannot be opened/read, is not a disk file - a FIFO/device would block `ReadFile` on the main thread forever, so `GetFileType` must say `FILE_TYPE_DISK` - or `GetPath` did not fit the 1024 B capture). The guard is a GRAMMAR check, not a bracket balance: it parses the KV3-text document the way Valve writes it (header, one root object, `key = value` members, comma-separated arrays, strings/raw strings/flags/numbers/literals/blob tokens, `//` and block comments) and refuses everything it cannot parse - including the three inputs that made pet's `ReloadDocument` recurse until the Wine main thread's stack overflowed (live 2026-10-13: `{{{ not kv3\n`, a valid header + truncated body, and a balanced document with a key that has no value; the process wedged and even SIGTERM did not stop it) and the wrong root class that exited fail-fast (c0000409) instead. Class/field names inside the document are deliberately not validated: pet tolerates unknown classes/fields. On refusal `ReloadDocument` is **not called**, one `logline` is written, and the document keeps its root. `discardedChanges` is the pre-call dirty state. `path` is captured from `GetPath` before the call. `reloaded` re-resolves the chain after the call and `hasRoot` is the **new** root; `hasRoot:false` means the document has no root at all (an empty/untitled document, or a load pet itself refused) - it is **not** the expected outcome for a broken file (the old "a failed load leaves the document with no root" claim is wrong). No prompt, no modal. |
| `pet.save` | implemented `[I] until live` | `NEED_PET` | `{force?:bool}` | `{saved:bool, skippedClean:bool, path}` | `session->Save(session, path, path, 0, 0)` (session slot `PETVS_SESSION_SAVE` = 18), then `SetModified(session, 0)` (slot `PETVS_SESSION_SETMODIFIED` = 12) only when it returned nonzero - exactly the FileSave handler's recipe | `-32003`; `-32015 UntitledDocument` when `isNewFile` (`session+PETOFF_SESSION_ISNEW`) **or** `GetPath` is empty (Save As would open a modal). A clean document skips the API call entirely (`skippedClean:true`, nothing is written) unless `force:true`, which saves it anyway (pet re-serializes the file in its own form). `path` is copied into a bridge buffer **before** the save (the path string may be reallocated). No modal on failure (pet logs `WARNING: Failed to save file`), reported as `saved:false`. |
| `pet.functionTypes` | implemented `[I] until live` | `NEED_PET` | `{}` | `{count:int, types:[{type:int, index:int, name:string, className:string, metadata:[string], obsolete:bool, singleton:null}]}` | `*(void **)(base+PETG_PARTICLE_MGR)`; per type 0..6 `GetFunctionCount(mgr, type)` (definition vtable byte offset `PETOFF_PMGR_FUNC_COUNT` = 0xC0) and `GetFunction(mgr, type, i)` (`PETOFF_PMGR_FUNC_GET` = 0xC8); per definition `GetName(def)` (`PETOFF_FACTORY_NAME` = 0x0, vtable **slot 0**) and `GetSchemaBinding(def, out8)` (`PETOFF_FACTORY_CLASSINFO` = 0x10) -> `SchemaClassInfoData_t` (`m_pszName`, `m_nStaticMetadataCount`/`m_pStaticMetadata`) | `-32017 {reason:"particle_mgr_null"}` when the manager cache is still 0 (it is filled at pet's Connect [I: unchecked live]); `{reason:"particle_mgr_unreadable"}` when the manager's vtable (or either slot byte offset the calls below use) is not readable; `{reason:"function_count_unusable"}` for a per-type count outside `0..4096`; `{reason:"function_definition_unreadable"}` for any unreadable definition, class-info pointer, or a name/metadata string that does not fit the bounded (128 B) buffer. **Never a partial list.** `obsolete` is the operator picker's own hide rule (`MObsoleteParticleFunction` in the class metadata); `singleton` is always `null` this pass (definition slot 1 returns the group - `-1` = "only one allowed" - but nothing consumes it yet). `GetFunction` does **not** bounds-check: the bridge only asks for `0 <= i < count`. Reply buffer 512 KB. |
| `pet.schema` | implemented `[I] until live` | `NEED_PET \| NEED_SCHEMA` | `{class}` 1..127 chars, `[A-Za-z0-9_]` only | `{class, found:bool, size:int, project:string\|null, bases:[string], fields:[{name, type, declaredIn}], metadata:[string]}`; `found:false` -> `size`/`project` **null**, `bases`/`fields`/`metadata` empty | `FindTypeScopeForModule(ss, "particles.dll", 0)` (the same slot 13 the game-state resolver uses) + `obj_is(SSV_TYPESCOPE_VT)`; class by name (`FindRawClassBinding`, slot 25) with an `m_pszName` equality check, then `m_nFieldCount`/`m_pFields` (stride `SchemaClassFieldData_t`) + `m_pBaseClasses` recursively (depth <= 8), `m_pStaticMetadata` | `-32023 SchemaUnavailable` when particles.dll has not registered its scope yet or resolves to the global scope; never the cached `client.dll` scope. **`found:false` is a result, not an error** (and `size` is `null` there - a consumer must treat it as optional). `type` is `null` for a field whose `CSchemaType::m_sTypeName` hop is unreadable. `fields` are already **flattened** (base fields included, each with its `declaredIn`), `bases` is the flattened chain in walk order. `metadata` is the class's static metadata list. `-32023` when an unreadable hop is hit mid-walk (the walk is rolled back - never a partial list). String copies are bounded at 128 B. |

### §7.1 pet load robustness (live 2026-10-13, build 2000930, after the Fable review)

What dodging the gate actually costs, observed live with `pet.reload` on a copy of `probe.vpcf` -
this is the reason the gate is a grammar check and not a bracket balance, and why it also covers the
root class:

| input reloaded | pet behaviour |
|---|---|
| `{{{ not kv3` | main-thread stack overflow, process wedged |
| valid kv3 header + truncated body (unbalanced) | stack overflow, wedged |
| BALANCED but invalid KV3 (`_class = ` then `}` on the next line: a key with no value) | stack overflow, wedged - **this passes the old bracket/string/comment lexer** |
| root `_class = "CParticleSomethingElse"` | process exits: unhandled exception `c0000409` (fail-fast) |
| unknown function `_class` in `m_Operators` | tolerated: the entry is silently dropped (count 2 -> 1) |
| `m_nMaxParticles = "abc"` (wrong field type) | tolerated |
| `m_Operators = 5` (function list not an array) | tolerated |

So the must-refuse set for any load is: KV3 grammar errors, and a root whose `_class` is not
`CParticleSystemDefinition`. Everything else in the document (unknown or misspelled classes and
fields, wrong field types) is pet's own business and is deliberately NOT checked here. A missing or
unreadable file is refused by `pet.open` only (pet's `OpenFile` pops a modal for it); `pet.reload`
reads the document's own path.


## §8 error codes — new

One new refusal under an existing code: **`-32602 InvalidParams` with message `UnloadableFile`** -
`pet.reload`/`pet.open` on a file the bridge's pre-load gate (`bridge/kv3_check.h`) refuses,
`data:{reason, path}` with `reason` one of `binary`, `no_kv3_header`, `not_vpcf`,
`unterminated_comment`, `unterminated_string`, `no_root_object`, `unbalanced`, `too_deep`,
`trailing_content`, `expected_equals`, `expected_value`, `bad_key`, `bad_token`, `comma_in_object`,
`expected_comma`, `no_root_class`, `wrong_root_class`, `too_large`, `read_failed`, plus the
`pet.open`-only path reasons `no_app_root` and `no_addon`. Otherwise m2.12 reuses `-32003
NoActiveDocument`, `-32015 UntitledDocument`, `-32016 UnsavedChanges`, `-32017 PrerequisiteFailed`,
`-32023 SchemaUnavailable`.

## §5/§6 notes

- Method count: 72 registered / 54 implemented (m2.11: 65/47; all seven new methods implemented).
- Modules: `PIN_MODULE_COUNT` is now 12; `pet.dll` appears in `control.capabilities.modules` /
  `control.ping.modules` / `control.version.hostModules` with the states `not_loaded` (the usual
  state until `pet.load` runs), `verifying`, `verified`, `mismatch`. `control.version.bridgeVersion`
  is `m2.12`.
- `pet.load` is the only method that works while pet.dll is absent; the other six answer `-32017
  PrerequisiteFailed {module:"pet.dll", check:"identity", state:...}` until it is verified
  (the generic gate, not a pet reason).
- Every new method copies engine-owned strings out into bounded bridge-owned buffers (`GetPath`
  1024 B, class/metadata names 128 B, open path 520 B) before any use; `pet.save` copies the path
  before `Save` (which may reallocate session strings) and refuses a `GetPath` that does not fit
  (a cut path would repoint the session).
- The `pet.open` `active` object and `pet.active` share one builder, so a session that died between
  the two calls is reported consistently (`null`/reason).
- Non-virtual, non-exported entries the bridge binds from pinned RVAs (byte-pinned 24-byte
  prologues, fail closed): `PETFN_ROOT_GETCHILDCOUNT` (0x11EBD0) and `PETFN_ROOT_GETCHILD`
  (0x11EBB0). `GETCHILD` has no bounds check, so only `0 <= i < count` is ever passed.
- `PETOFF_FACTORY_NAME` is a **literal** `0` in the signature DB (slot 0 has no displacement byte
  to read); the bridge uses it only as a vtable byte offset, never as a struct field offset.
- G1/G2: `bridge/kv3_check.h` (pure portable C: no headers, no libc, no Windows types; indexed
  memory, a NUL byte is a refusal, nesting cap 256) is the pre-load gate `pet.reload` and
  `pet.open` run before the pet call. It exposes two entry points: `kv3_check` (header + the
  KV3-text grammar, any `format:`) and `vpcf_check` (`format:vpcf` + grammar + the root `_class`).
  `bridge/test_kv3_check.c` is its HOST unit test, compiled and run by `build.sh` (the real
  fixtures are passed as an argument and read at test time - a missing one fails the test) before
  anything is built for Windows, so a checker regression fails the build; it covers the three
  live-crash inputs byte-exactly, the root-class and flag rules, and the pet.open path
  derivation (`-addon mcp_test`, `Z:\app\game\bin\win64\cs2.exe` -> `Z:\app`, the host
  path - 105 cases). `bridge/test_kv3_corpus.sh` (not wired into `build.sh`) is the
  real-content proof: it builds a small host runner and checks every `<!-- kv3` file under the
  workshop install's `content/` with `kv3_check` and every `*.vpcf` with `vpcf_check`
  (`_toolsautosave/` and `mcp_*` files excluded) - all must be accepted (101 + 3 on 2026-10-13).
  No pins change; `BRIDGE_BUILD` stays `m2.12`.
- `pet_open_gate` keeps its frame under 4 KB on purpose (the freestanding Windows build has no
  `__chkstk` for clang's stack probe): `pet_app_root` is `noinline` so its 1 KB exe-path buffer
  does not join the gate's three. `pet_read_vpcf` refuses anything `GetFileType` does not call a
  disk file.
- Original live-probe list (to be checked by the orchestrator, `bridge/test_pet.py`):
  `CreateInterface("ToolSystem2_001")` returns the `PETG_APP_OBJ` identity; `LoadToolSystem`
  behaves like `tool_request pet` (the session keeps draining) and pet.dll verifies; `GetPath`
  produces `Z:\app\...`-style Wine paths; the registry's type-1 class names match the open
  document's `m_Emitters` classes (the `GetSchemaBinding` deref cross-check); a broken file is
  refused by `pet.reload` with `-32602 UnloadableFile` (the document keeps its root and the main
  thread keeps draining) instead of being loaded; `pet.save` on a clean document writes nothing.
