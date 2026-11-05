# bridge/ — the bridge (`cs2_bridge.dll`, the Rust crate `crates/cs2-bridge`)

The bridge runs **inside** `cs2.exe -tools` (a tool plugin admitted through
`sdkenginetools.txt`) and exposes the loopback JSON-RPC endpoint both MCP servers talk to. It is
the Rust cdylib `crates/cs2-bridge`, built for Windows (Wine) and Linux by `bridge/build-rs.sh`
and installed into the shared Wine tools install by `bridge/install-rs.sh`.

The hand-written C bridge was retired at the M5 cutover (base commit
`d6e2f26037c3afd529e60e1e31d949b05bf61dc1`, m2.20 C + m3.10 Rust). Its source stays in the tree
as the **frozen m2.20 reference**: `mcp_bridge.c`, `mcp_bridge_*.inc`, `s2sdk.h`, `json_text.h`,
`kv3_check.h`, `map_path.h`, `test_entity_json.c`. It is never compiled or shipped, and never
edited. It stays because the Rust code cites it by line (`mcp_bridge.c:NNNN`), and because
`crates/s2sdk/s2sdk-sys/gen/gen.py --check` and the `crates/cs2-bridge` differential tests read
it. Its build scripts, `.def` files and the C host tests ported to Rust are gone (`git log` up to
the base commit has them).

Protocol: [`../../idabins/docs/data/bridge-protocol.md`](../../idabins/docs/data/bridge-protocol.md)
(method table §7 mirrors `crates/cs2-bridge/src/registry.rs`; keep them in step).
Rules: [`../../idabins/docs/WORKFLOW.md`](../../idabins/docs/WORKFLOW.md) §11 (never patch Valve
code; fail closed on pin mismatch; `[V]` claims only).

| File | Role |
|---|---|
| `../crates/cs2-bridge/` | the bridge: IPC thread (NDJSON over TCP), main-thread drain via the tools frame hook, `registry.rs` (the method table), the `ops/<group>` handlers, module pins (`src/generated/pins.rs`) |
| `bridge_pins.h` | **generated** by `pins.py` from `../sigs/signatures/<module>.json`: only what the SDK headers cannot give - module hashes, vtable/function/global RVAs, Qt chains, literals and the not-yet-RE'd items (170 `#define`s: `grep -c '^#define' bridge/bridge_pins.h`; 295 before the de-hardcode project). Slots, member offsets, struct sizes and consts of anything the headers describe are **not** here: an entry carrying `sdk` is never emitted. Committed; `pins.py --check` refuses a stale one (build-rs.sh runs the check). Kept for `sdkcheck.py`, gen.py's ties and history; the Rust bridge reads its own copy `crates/cs2-bridge/src/generated/pins.rs` that `pins.py --rust` writes alongside it |
| `s2sdk_gen.h` | **generated** by `crates/s2sdk/s2sdk-sys/gen/gen.py` (`emit_c_header`, from the MSVC sub-objects of the committed IR) and the owner of every slot, offset, size and const the bridge compiles: `S2_SIZEOF_/S2_ALIGNOF_/S2_OFF_` for every record and spec, `S2_SLOT_<Class>_<Method>[_N]` and `S2_VTLEN_` for the MSVC vftables (`_N` is the *declaration* index of an overload; MSVC packs a group in reverse, so `StartSoundEvent_2` is slot 11), `S2_CONST_/S2_IFACE_` for the measured consts, and padded structs (with `_Static_assert` on every offset) for the records bridge code reads member-wise. Freestanding (no `#include`). It asserts nothing against `bridge_pins.h`: the sigs entry's `sdk` field is the binary measurement `pins.py` and `sigs/scripts/sdkcheck.py` hold the header to. `gen.py --check` refuses a stale one; never hand-edit. The Rust bridge reads the same IR from `s2sdk_sys::generated` |
| `s2sdk_gen_ties.h` | **generated** by the same gen.py (`emit_c_ties_header`): the late ties against values `s2sdk.h` owns below the include site. Kept as the gen.py `--check` artifact and for history; the Rust bridge carries no C TU any more |
| `pins.py` | the generator; emits `bridge_pins.h` **and** `crates/cs2-bridge/src/generated/pins.rs` (`--rust`) - the Rust bridge's pin source; reads the sig DB through `../sigs/scripts/sigcheck.py`'s resolver so header and `sigcheck resolve` can never disagree. For an entry with `sdk` it emits no `#define`, keeps the `k_pinSlots`/`k_pinBytes` runtime rows, and refuses to write when the `sdk` expression (evaluated against `s2sdk_gen.h`) differs from the binary measurement |
| `mcp_bridge.c`, `mcp_bridge_*.inc`, `s2sdk.h`, `json_text.h`, `kv3_check.h`, `map_path.h`, `test_entity_json.c` | the frozen m2.20 C bridge (see the note above): never compiled or edited; the Rust code cites it by line, and `crates/s2sdk/s2sdk-sys/gen/gen.py --check` (the ties and the generated-name disjointness check) and the `crates/cs2-bridge` differential tests (`tests/data/*_gen.c`, `tests/entity_json.rs`) read it |
| `build-rs.sh` | builds the Rust bridge for both targets (`cargo build --release -p cs2-bridge`, x86_64-pc-windows-msvc + x86_64-unknown-linux-gnu, memory-bounded cargo form per AGENTS.md), stages `target/bridge-rs/{win64/cs2_bridge.dll,linuxsteamrt64/libcs2_bridge.so}`, and gates each on `check-exports.sh` (exactly the `CreateInterface` export). Runs `pins.py --check --rust` first: a stale `bridge_pins.h` or `pins.rs` fails the build before any cargo work |
| `install-rs.sh` | installs the Rust bridge (`target/bridge-rs/win64/cs2_bridge.dll`) into the scratch tree and adds the `cs2_bridge` manifest entry to `sdkenginetools.txt` (read at toolframework2 Init -> restart), plus the Metamod layout in its standard `csgo/addons/metamod` tree and the launcher (`bin/win64/{cs2mcp_launch.exe,cs2mcp_mmload.dll}`); no `gameinfo.gi` edit (`--metamod` is a no-op alias) |
| `runtime.sh` | `up \| open \| test \| cycle \| status \| logs \| down` for the podman Wine tools runtime (build + install first: `build-rs.sh`, `install-rs.sh install`) |
| `check-exports.sh` | the export gate `build-rs.sh` and `install-rs.sh` run on the built DLL/SO: exactly one exported symbol, `CreateInterface` |
| `test_*.py`, `geo_rpc.py` | live bridge tests (run through `runtime.sh test` / directly against `session.json`); the map-editing tests follow the run-unique identity + cleanup conventions in "Live test conventions" below; `test_modeldoc.py` is the ModelDoc first-pass live test the ModelDoc orchestrator runs (`test_client.py` helpers; poll `modeldoc_editor.dll` -> verified first; `--static` checks the m2.14 render sources plus the current build string offline), `test_modeldoc_render_live.py` the `modeldoc.render` PNG check (19 checks, open the model addon-relative first: `runtime.sh open models/<x>.vmdl`); `test_pet.py` is the pet suite (poll `pet.dll` -> verified; 17/17 in the m2.13 run, 12 `pet.*` methods advertised), `test_pet_live.py` the pet second-pass live tests (core, plus `--render`, `--namedvalue`, `--depth` modes; live edit + undo + refusals), and `test_pet_render.py` the `pet.render` PNG check (320x240 host PNG, stats, negative params); `test_props.py` is the m2.18 prop suite the props orchestrator runs (model draw + real value undo through `view.render` crc polls, `node.setTransform`, the `entity.addConnections`/`removeConnections` round trip with undo, the param refusals; `--static` checks the m2.18 source contract offline), `test_triggers.py` is the m2.19 trigger suite the triggers orchestrator runs (tie via class and via entityID, the allowRetie retie matrix with the emptied-entity deletion, `entity.get` children, `entity.untie` + undo, the refusal matrix with a FRESH doc.active baseline per row (FIX-B L2) and the all-`alreadyTied` success no-op row (FIX-B L1: no group, doc unchanged); the documented no-op group of the point-class refusal, whose error carries `undoGroupOpened`/`undoLabel` (FIX-B L3); rows a session cannot run print an explicit `NOT RUN` line (the locked-old-parent retie - no method sets a lock; the prefab/instance gate - no prefab in the scratch map); `--static` checks the m2.19 source contract offline, source shape only, with EVERY refusal's check text order-verified against `undo_open`); `test_unblock_rows.py` is the m3.12 suite (`--only version|mesh|selection|jobs`: live `mesh.get` read-only + undo-follow, `selection.*` nodes + faces with the doc unchanged, `job.*` resourcecompiler jobs incl. cancel with no compiler left behind, `--full-map` a full map compile to the OK line; `--static` checks the m3.12 rows, handlers and pins offline) |
| `bridge_build.py` | the facts the `test_*.py` scripts check against, read from the Rust crate: `expected_build()` (`BRIDGE_BUILD` in `crates/cs2-bridge/src/lib.rs`), `methods()` (`registry.rs`), `handler_names()` (`ops/` HANDLERS), `pin_names()` (`generated/pins.rs`) |
| `vmapreader/` | `.vmap` (DMX) reader the map-editing tests use to check the saved file |
| `protocol-diff-m2.*.md` | the C bridge era's `bridge-protocol.md` §7/§8 change notes (m2.9-m2.20; history) |
| `protocol-diff-m3.*.md` | the Rust bridge's change notes (one per `m3.N` wave: the ported groups, every wire difference from the C the port could not avoid, and which `test_*.py` cases cover the group) |

## Live test conventions (`test_*.py`)

The map-editing tests share an isolation convention so a run's verdict never
depends on what earlier runs left behind (2026-10-12: probe.vmap had grown to
~70 entities with 4 stale hits per reused name/origin, and a same-session undo
baseline of depth 11):

- **Run-unique identity.** Every test tags what it creates with
  `RUN = "<time hex><pid hex>"`; targetnames become `mcp_kv_<RUN>`,
  `mcp_batch_<RUN>`, ... and file checks match exactly this run's names (the
  `== 1` assertions stay). Origins are jittered per run on the /16 grid
  (`RUN_OFF`), with each test in its own 20000-unit coordinate band so one
  test's leftovers can never sit on another test's check origins. Leftovers
  from pre-convention or aborted runs keep their plain names/fixed origins
  forever and can no longer match anything; a freak jitter collision fails
  safe (extra hit → `len > 1` → FAIL), never the other way.
- **Cleanup.** Each test best-effort reverts what it created (undo only groups
  whose name matches what it itself opened - it stops at anything else) and
  saves again, so probe.vmap stops growing. Cleanup results are printed but not
  part of the pass criteria; a mid-test abort can leave this run's entities
  (harmless: run-unique). Note map.save does not open an undo group, so the
  group counts per test are exactly its mutating calls.
- **`test_undo` drains first.** The `undone:false` property is only observable
  on an empty stack, and `undo.undo` also answers `undone:false` when a pop
  fails - so the test pops groups (bounded loop + deadline) until
  `doc.active` reports `undoDepth == 0` and asserts that. The undo history is
  per-document and holds only this session's undoable (automated MCP) edits,
  so draining cannot touch anything the test relies on and doubles as leftover
  cleanup. Caveat: it reverts ALL pending undoable edits of the active
  document - do not run it in a Hammer session whose manual edits you want to
  keep.
- **Which map, which asset name.** `runtime.sh open <asset>` takes the addon-relative asset
  (`maps/probe.vmap`, `maps/defuse_poc.vmap`); an `mcp_test/maps/...` name raises Hammer's
  "Unrecognized Asset" modal, which parks the main thread (every queued request then answers
  `QueueTimeout`, `control.ping` shows a growing `mainThreadAgeMs`). The file-checking tests
  (`test_set_keyvalues`, `test_edit_batch`, `test_undo`, `test_geometry*`) parse
  `maps/probe.vmap`; `test_entities`/`test_props`/`test_triggers`/`test_entity_create`/
  `test_reload` want `maps/defuse_poc.vmap`.
- **Console commands in a parked harness.** `console.exec` exists only in `-testscript`
  and `-tools` sessions (m3.13), and while the vtest harness sits in `Test_WaitForCheckPoint` the engine's command
  buffer is held: a queued command runs only after the script moves on. Put the commands a test
  needs into its own test script between checkpoints (`test_metamod_coexist.py` writes one),
  and release each checkpoint with `test.checkpoint`.

## Signature DB and binaries

The signature database is `../sigs/` ([../sigs/README.md](../sigs/README.md)). The resolver reads the
pinned DLLs from `$CS2_BINS` (default `<repo>/target/game-bins`, gitignored); fill it from the Wine
install once per checkout:

```sh
python3 sigs/scripts/stagebins.py --game "${CS2_MCP_PROJECT_ROOT:-.}/workshop/run/app" --out target/game-bins
python3 sigs/scripts/sdkcheck.py         # the generated SDK numbers vs the binaries
./bridge/build-rs.sh                     # pins check -> build both targets -> export gate
./bridge/install-rs.sh install           # DLL + sdkenginetools.txt manifest entry
./bridge/runtime.sh cycle                # up -> open -> test
```

`runtime.sh` and `install-rs.sh` read the project root from `CS2_MCP_PROJECT_ROOT` (default: the
repo's main checkout, `bridge/lib-root.sh`, so a worktree shares it; `CS2DOCKER_ROOT` is the
deprecated alias) for the `workshop/run/app` tree and `.podman/`, and `runtime.sh` the container
image from `IMG` / `CS2_MCP_IMAGE` (default `ghcr.io/personcopperfine/cs2-docker:latest`); the
Python tests use the same rule (`bridge/cs2mcp_root.py`). See the header of each script.

## Install/uninstall/status + launch flags

`install-rs.sh` installs the bridge + the Metamod layout + the launcher (`crates/mm-launch`;
Metamod is a hard dependency; `--metamod` is kept as a no-op alias) into the shared Wine tools
install and lets `runtime.sh` load it:

```sh
./bridge/install-rs.sh install              # tool DLL (ONE image) + manifest entry + Metamod (standard
                                            # addons tree) + cs2mcp_launch.exe/cs2mcp_mmload.dll
./bridge/runtime.sh up                      # runs install-rs.sh ensure first; loads cs2_bridge;
                                            # adds -insecure itself (Metamod refuses without it)
./bridge/install-rs.sh status / uninstall   # what is installed (sha256); restore + remove
```

**Launching.** The Wine sessions start `cs2mcp_launch.exe`, not `cs2.exe`
(`bridge/runtime.sh`: `entrypoint.sh cs2mcp_launch.exe -tools ...`; `docker.rs` `wine_command` /
`wine_client_command` for the client-mcp launches). The launcher starts `cs2.exe` suspended,
injects `cs2mcp_mmload.dll`, and the DLL redirects the one `LoadLibraryExW` of
`...\csgo\bin\win64\server.dll` to the Metamod loader; a failure exits 3 with
`[cs2mcp-launch] FAIL: ...` instead of running without Metamod. The Linux client and dedicated
sessions prepend `csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so` to `LD_PRELOAD` on
the `cs2.sh` command only (`client-mcp/docker/linux-*.sh`) and refuse to launch when it is
missing. **No `gameinfo.gi` edit is needed on either platform**, and a real Windows /
Steam-launched retail CS2 is not a target (containers only). Details, the layouts and the spike
evidence: [metamod/README.md](metamod/README.md) "Launching" and "The layout (both platforms)".

`install` gates the DLL on `check-exports.sh` (exactly the `CreateInterface` export) before
touching anything, backs up every pre-existing file to
`<cs2-mcp>/target/bridge/backup-rs-<UTC>/` first, is idempotent (re-runs change nothing), and
`uninstall` restores the oldest backup (the pre-bridge original) (`cmp`-verified) removing only what install added.
`ensure` is the launch-path command: a read-only per-artefact check (`ok` / `missing` /
`stale: <why>`, exit 0 + untouched when all current), otherwise it refuses (exit 4) while any
running container mounts the tree or runs `install` to repair (backup first); "no built DLL and
nothing installed" fails `build it first` (exit 1). `--dry-run` prints everything it would do
and changes nothing (works for `ensure` too).

`install`/`uninstall`/`ensure`-repair refuse while ANY running container mounts the tree (wine: a
bind of the app dir or a parent/child of it; a failing podman also refuses, naming the problem).
`--platform linux` targets the cs2-home volume's Linux game dir instead - the STANDARD
`csgo/addons/metamod` layout (client + dedicated; no gameinfo edit; the Linux client lane adds
`-insecure`), run through `podman unshare` with backups under `backup-linux-<UTC>`. `CS2_GAME` /
`CS2_LINUX_GAME` point it at a test tree (layouts: [metamod/README.md](metamod/README.md)
"Launching" and "The layout (both platforms)").

## Hooks: KHook on the class's shared vtable slots, and bridge-owned listener objects

The bridge does not copy vtables. `input.*`, `sound.*` and `game.saveRestore` install KHook
virtual hooks (`SetupVirtualHook`, `SetupMode::Sync`) on the class's **shared** vtable slot -
`CCSGOInput::unk3021`/`unk3023` (the frame-begin slot and the view-angle override),
`CSoundOpSystem`'s three `StartSoundEvent` overloads and
`ISource2ServerConfig::AllowSaveRestore` - through the generated slot
indices and the pinned tables; `s2sdk::khook_thunks!` generates the per-signature thunks. KHook
patches the entry in place and never restores it (`third_party/khook/src/detour.hpp:96-104`,
`SetupVirtual`), so
every object of the class and every plugin that hooks the slot later sees the same entry; the
bridge's identity check asks KHook for the saved original
(`crates/cs2-bridge/src/modules.rs` `slot_matches`), so the pinned slots still identify the
object.

Every callback is passive by default: it answers `Ignore` unless `this` is the recorded object
(all three targets are singletons), and it answers `Ignore` at once when its arm generation was
retired (a teardown whose removal KHook deferred; input, sound and save/restore each carry one,
bumped on arm and again first thing on disarm). Only the view-angle override (`unk3023`) and
`AllowSaveRestore` are value changers, and they **Supersede** only while armed; a second value
changer on the same call is reported per DESIGN §13 (a rate-limited `khook conflict:` line, a
`conflicts` counter in the reply, a caught `debug_assert!` in debug builds) - never fought: a
plugin whose pre runs first wins an equal `Supersede`, and that is the report's "ahead" case.
The three sound start hooks are post observers (they record the original's guid) and change no
value. The sound listener (`AddSoundEventListener`/`RemoveSoundEventListener`) is a
**bridge-owned** C++ object whose one word points at a bridge-owned 16-slot table: registering
it changes nothing in a game table. (Up to m3.10 the registered pointer was the table itself, so
soundsystem's listener walk called through code bytes and `{"mode":"listener"|"both"}` crashed
the game on the next sound; fixed in m3.11.) Listener mode is still **unproductive**: soundsystem
calls slot 3 (`OnLibraryStackFieldsMessages`) on its listeners, and slot 5
(`OnSoundEventStarted`, the one the bridge implements) delivered no event live (2026-10-20, 7000+
wrap events in the same window) - use `wrap`. The old `crates/cs2-bridge/src/vtswap.rs`
per-object vptr copy is gone. The per-wave details and C references live in
`bridge/protocol-diff-m3*.md`.

**Coexistence, live** (`bridge/test_metamod_coexist.py`: the bridge plus the
`crates/mm-testplugin` fixture on `AllowSaveRestore`, modes `passive`/`supersede0`/`supersede1`,
both hook orders in one session). Passive hooks of both plugins fire on every call and change
nothing. KHook runs the **newest** hook's pre first, and of two equal `Supersede`s the first saved
value wins - so the newer hook's value wins. When the other plugin's value wins, the bridge
counts and logs the conflict (`ahead`). When the bridge's value wins, the other plugin's
`Supersede` was already discarded by KHook before the bridge's post runs and nothing KHook
exposes shows it: **that order counts no conflict** (the other plugin sees the final value in its
own post). Run it after any change to the hook layer: `bridge/build-testplugin.sh`, then
`bridge/test_metamod_coexist.py` (no other `cs2tools` container; ~3 min).

## The Metamod plugin table (`metamod.plugins`, m3.11 lane B4)

`metamod.plugins` is the bridge's first Rust-only method row: it is NOT in the frozen C
table (`registry.rs` `METHODS`, pinned row for row by the differential) but in
`RUST_METHODS`, and it is a **control-plane** row (`needs == 0`) answered on the IPC lane
from a snapshot (`crates/cs2-bridge/src/ops/metamod.rs`) - no queue slot, no tick, so the
answer comes even when the main thread is wedged. The lane route is
`wiring::BridgeState::route`'s intercept, not the C's `find_method` chain.

- **Snapshot.** `Load` fetches `ISmmPluginManager` through
  `MetaFactory("IPluginManager")` (generated `MMIFACE_PLMANAGER`; a refusal is logged and
  `Load` still succeeds) and the table is rebuilt on the main thread at `Load`,
  `AllPluginsLoaded`, `OnPluginLoad` and `OnPluginUnload`: ids `Pl_MinId(1)..=max_seen+16`
  (the bound is documented - `m_LastId` is not reachable through the interface), gaps
  skipped, `file` copied out of Metamod's pointer immediately, statuses named from the
  generated `Pl_*` consts (`unknown(<n>)` for a value they do not name), `self` = our id.
  `Unload` forgets the manager and the snapshot.
- **Reply.** `{"plugins":[{"id":N,"file":"...","status":"running","source":0,"self":true},...],
  "generation":N,"rebuiltAtMs":T}`; `-32017 PrerequisiteFailed` with
  `{"blockedOn":"metamod: not loaded"}` when Metamod is absent, or
  `{"blockedOn":"metamod: IPluginManager unavailable"}` when `Load` could not get the
  manager (the `NEED_METAMOD` bit, documented next to `NEED_KHOOK`, names the prerequisite
  the gate cannot ask - no row carries it; the row answers itself).
- **Tests.** Host: `crates/cs2-bridge/tests/metamod_plugin.rs` (mock `ISmmPluginManager`;
  the NEGATIVE pins that the method never calls the manager off a callback thread). Live:
  `bridge/test_metamod_plugins.py` (our row `self:true`/`running`/the bridge's own file;
  with `CS2MCP_TESTPLUGIN=1` the `mm_testplugin` row too).

## Console variables (`cvar.get`, m3.13)

`cvar.get {names:[..]}` reads cvars from the engine's `ICvar` (`VEngineCvar007`, tier0): `FindConVar` through
the MSVC hidden return pointer, `GetConVarData` with the `ConVarRef` by value, members from the generated
`ConVarData`. The object is held to the pinned `CCvar` vtable (`T0V_CCVAR_VT`, slots `T0VS_CVAR_FIND` /
`T0VS_CVAR_GET_DATA`). Wire contract and the pointer table: `protocol-diff-m3.13.md`. Registered in
`RUST_METHODS` (`NEED_TIER0`), body in `ops/cvar.rs`. `console.exec` also accepts `-tools` sessions since m3.13.
No live test script yet; the live checklist is `workflows/streamline/FIXME.md` (WP1.3).

## Job inputs (`job.start`, m3.16/m3.17)

`job.start` runs `resourcecompiler` on one content-relative input (`ops/job.rs`: `INPUT_EXTENSIONS`, `PLAIN_FLAGS`).
m3.16 added the map-stage flags (`-world -phys -vis -bakelighting`), m3.17 the `js` extension: a cs_script source
(`maps/scripts/<name>.js`) compiles to its `.vjs_c` (hammer-mcp `cs_script {action:"compile"}` / `compile_asset`).
Notes: `protocol-diff-m3.16.md`, `protocol-diff-m3.17.md`. The three allowlists (bridge, hammer-mcp
`jobs.rs`, the mock in `cs2-mcp-common`) stay equal.

## Per-face texture alignment (`mesh.setFaceTexture`, m3.14)

One write method over four hammer.dll pins (`HFN_MESH_{SET,GET,JUSTIFY,ALIGN}_FACE_TEXTURE`): align to the
world grid / the face plane, justify, and absolute shift/scale/rotate, per face, one undo entry. Wire contract:
`protocol-diff-m3.14.md`. Registered in `RUST_METHODS` (`registry.rs`), body in `ops/mesh.rs`. There is no
live test script for it yet; the live checklist is in the protocol diff.

## Adding a method (the procedure both servers rely on)

1. A new virtual slot, member offset, struct size or const is a **header declaration first**: add
   the method/field to the s2sdk headers (a fork commit in `crates/s2sdk/upstream`, `unkXYZ` names
   and `AMNOTE:` per its AGENTS.md, no slot numbers in comments), bump `UPSTREAM.lock`, run
   `python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py`. The Rust side spells the generated
   `s2sdk_sys::generated::<module>::<Class>` types, `<Class>_slots::<m>` consts and the generated
   typed vtable fields (`<Class>::vtable(p).<m>`) - never a hand-written number.
2. The **binary measurement second**: reverse the call path in IDA and record it as an entry in
   `../sigs/signatures/<module>.json` (`../sigs/ida/ida_sigs.py` generates the locator;
   `sigcheck.py resolve --module <m>` must give the value IDA shows) carrying the `sdk`
   expression, written with `python3 sigs/scripts/sdkadopt.py PIN=S2_EXPR` (it checks the macro
   exists and equals the measurement). `sdkcheck.py` must show the new `sdk:` row as PASS. Only a
   number the SDK cannot give (an RVA, a Qt chain, a literal) gets a pin; then
   `./bridge/pins.py --rust crates/cs2-bridge/src/generated/pins.rs` regenerates the Rust pins
   (and `bridge_pins.h`; commit both) and the module goes into `MODULES` / `PIN_*` if it is new.
3. Implement the handler in `crates/cs2-bridge/src/ops/<group>.rs` and add one line to
   `ops/mod.rs`'s `GROUPS`; the row already exists in `registry.rs` (the gate runs before the
   body, exactly as the C's dispatch did).
4. Write the protocol note (`bridge/protocol-diff-m3-<group>.md` or the next wave's file) and add
   the §7 row in `bridge-protocol.md` (idabins) with `[V runtime]` evidence from
   `runtime.sh cycle` / a `test_*.py`. Bump `BRIDGE_BUILD` in `crates/cs2-bridge/src/lib.rs`.
5. Consume the method from a server tool (capability-gated); the servers never see addresses.

Game update: [../workflows/game-update.md](../workflows/game-update.md) lane B (`pins.py` refuses to
run until every pinned module is recorded under `sigs/CURRENT_BUILD`; `PIN_GAME_BUILD` is emitted
from it).

## The GameFrame tick (Metamod-only images)

The tools build (`cs2.exe -tools`) is ticked by the tools framework's `IToolSystem2::RunFrame`.
The Linux dedicated server, the Linux client and the Wine client have no tools framework, so
`crates/cs2-bridge/src/gameframe.rs` gives them the bridge's tick from the one per-frame callback
the server DLL offers: `ISource2Server::GameFrame` (the generated slot
`ISource2Server_slots::GameFrame`), hooked with KHook on the object's own vptr at Metamod `Load`.
The callback is post-only and answers `Ignore`; it runs `wiring::tick` (listener start, module
poll, queue drain) unless this image serves the `tool` role - checked per call, because under the
Wine tools the tool object appears after `Load` and the tools framework must stay the only tick.

What an operator sees:

* `Load: tick installed (ISource2Server::GameFrame -> wiring::tick)` in the bridge log;
* `gameframe tick N` for the powers of two (the tools tick's own rule: 1, 2, 4, 8, ...);
* `McpBridge_001` v3: `"tickSource":"tools"|"gameframe"|"none"`, `"gameframeTicks":N`,
  `"gameframe":"installed"` or the reason it is not (`"unavailable: ..."`), and
  `"gameframeOffThread":true` once a frame arrived on a thread other than `Load`'s (the status
  still names the hook as the source, but it is not producing ticks). The v2 members
  (`metamod`, `khook`, `tick`, `roles`) are unchanged in name, order and meaning.
* `GameFrame` only runs while a server runs: always on a dedicated server, on a client only with
  a map loaded. **A client at its main menu does not tick** (the counter freezes; there is no
  listener until a frame arrives). **A hibernating dedicated server does not tick either** (live
  2026-10-20: no frame at all while empty with `sv_hibernate_when_empty 1`, the default);
  client-mcp's `server_up` therefore passes `+sv_hibernate_when_empty 0` unless `hibernate:true`.
* **Linux: the `.so` stays mapped after `meta unload` once it has ticked.** The tick runs std code
  on the game's main thread that registers a thread-local destructor
  (`__cxa_thread_atexit_impl`), and glibc then refuses to unmap the library while that thread
  lives. A later `meta load` works but reuses the old image (statics carry over - the tick
  counter continues), so **replacing the `.so` needs a server restart**. A server that never
  ticked unmaps normally.
* The tick is main-thread only. With `sv_tick_parallel_with_client` on, libengine2 can run the
  server frame as a `"ServerGameFrame"` pool job, so slot 19 is called from a worker; the
  callback logs `gameframe: GameFrame off the main thread (sv_tick_parallel_with_client?) - not
  ticking` once and leaves that frame alone (the bridge's tick, and every op it drains, is
  engine-thread work). `"gameframeTicks"` stops counting while that is the engine's mode, and
  `"gameframeOffThread":true` says so in the status line.
* A **Metamod unload in an mm-only image stops the listener and the queue before the image goes
  away**: Metamod `dlclose`s the DLL once `Unload` answers true, so the accept/writer threads
  cannot be left in it, and `session.json` is deleted (`meta unload` -> hosts see the bridge
  gone). A later `meta load` in the same process starts a fresh listener and serves again (the
  one-shot and the queue's shutdown are reset by the new `Load`). In a `-tools` image the tools
  framework owns the listener and a Metamod unload leaves it running. If a listener thread
  outlives the stop deadline the unload is **refused** with
  `UnloadTimeout: listener thread still running` (the thread stays owned by the listener, so a
  retried `meta unload` joins it once it exits); the plugin is then inert - runtime and GameFrame
  hook down, verifier stopped, no tick - until that retry succeeds or the process restarts. That
  refusal keeps the KHook API installed (a removal queued before it still has to run); only a
  successful unload forgets it.
* An unload issued *from inside the frame* - another plugin's `GameFrame` hook calling into
  Metamod is the way that happens; `meta force_unload`, `meta clear` and `UnloadAll` are *forced*
  paths (Metamod ignores our `false` for them) and are not in-frame paths - runs the full
  teardown and skips only the synchronous hook drain: that stack holds the GameFrame hook's
  KHook capsule, and the drain would spin on it. Metamod's own `Unloader` re-issues every hook
  removal asynchronously and holds the `dlclose` until each has reported back, so the pending
  GameFrame removal is finished before the image is unmapped. The in-frame check is made twice
  (at the entry and just before the drain), but a frame that starts on a pool thread in between
  makes the re-issued synchronous removal wait **one frame** (KHook's capsule mutex) instead of
  the unload deadline; `s2sdk::hook` has no per-id liveness query to wait for the queued
  GameFrame removal first. That one-frame wait is the only teardown wait the deadline does not
  bound.

Ops from the GameFrame tick: the tick runs the queue drain inside a KHook `post` thunk, and from
inside a thunk `s2sdk::hook` takes its hook gate with `try_lock` only - waiting there could
deadlock against a synchronous removal spinning on this very call's capsule. So a registration
from the tick succeeds when the gate is free, and is refused with `GateBusy` when another thread
holds it at that moment. In this build the registration is `game.saveRestore`'s enable: a
`GateBusy` answers `-32017 PrerequisiteFailed` with `{"blockedOn":"khook: retry next tick"}`, and
a disable is accepted - it clears the armed flag and defers the removal to the asynchronous path
(`Drop`), whose removal function KHook's delete thread runs. Every other op the drain can reach is
unaffected. The tools host has no such limit: its `RunFrame` is outside any thunk.

## Where the log and `session.json` live

Both paths follow one rule per host, and the bridge log and the discovery file always share a
directory (`crates/cs2-bridge/src/log.rs` and `src/ipc/discovery.rs` resolve it through the same
helper, `discovery::linux_dir`). **Wine/Windows**: the log is `Z:\app\mcp_bridge_rs.log` (the C
bridge's directory, Rust name; `CS2_BRIDGE_LOG` overrides on every host) and `session.json` is
`%LOCALAPPDATA%\cs2mcpbridge\session.json`, which the host reads as
`<pfx>/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json`. **Native Linux**:
`$CS2_BRIDGE_LOG`, else `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log`, else
`$HOME/.local/share/cs2mcpbridge/bridge.log`; `session.json` follows the same directory rule
(`$XDG_RUNTIME_DIR/cs2mcpbridge/session.json`, else `$HOME/.local/share/cs2mcpbridge/session.json`)
- the `$HOME` fallback is what runs on the Linux dedicated server, whose container has **no
`XDG_RUNTIME_DIR`** (`HOME=/home/steam`), so both files land in
`/home/steam/.local/share/cs2mcpbridge/` (inside the `cs2-home` volume the container mounts at
`/home/steam`). The directory is created `0700` (whichever of the log or the discovery writer
gets there first), the token file `0600`, and a discovery directory that grants group or other
access is refused (fail closed). Before the `$HOME` log fallback existed, a container without
`XDG_RUNTIME_DIR` wrote no log at all - a missing log there means "unreadable", not "nothing
happened".
