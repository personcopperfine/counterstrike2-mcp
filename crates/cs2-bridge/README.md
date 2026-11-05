# crates/cs2-bridge — the bridge (`cs2_bridge.dll` / `libcs2_bridge.so`)

The bridge runs **inside** the game: as a tool plugin in `cs2.exe -tools` (admitted through
`sdkenginetools.txt`) and as a Metamod:Source plugin in the client and dedicated-server images.
It exposes the loopback JSON-RPC endpoint both MCP servers talk to. It is a Rust cdylib built for
Windows (Wine) and Linux by `cargo bridge build` and installed into the game trees by
`cargo bridge install` (both in [`crates/cs2-bridge-tools`](../cs2-bridge-tools)).

Protocol: [PROTOCOL.md](PROTOCOL.md), the wire contract of the current build (the method table
mirrors `src/registry.rs`; keep them in step).

The bridge was first written in C. That C bridge (m2.20) is retired and no longer in the tree.
Comments that cite `mcp_bridge.c:NNNN`, `mcp_bridge_*.inc:NNNN`, `s2sdk.h:NNNN`, `json_text.h`,
`kv3_check.h` or `map_path.h` refer to it as of commit `8cdea42`:
`git show 8cdea42:bridge/mcp_bridge.c`.

| Where | Role |
|---|---|
| `src/` | the bridge: IPC thread (NDJSON over TCP), main-thread drain via the tools frame hook or the GameFrame tick, `registry.rs` (the method table), the `ops/<group>` handlers |
| `src/generated/pins.rs` | **generated** by `cargo bridge pins` from `../../sigs/signatures/<module>.json`: only what the SDK headers cannot give - module hashes, vtable/function/global RVAs, Qt chains, literals and the not-yet-RE'd items. Slots, member offsets, struct sizes and consts of anything the headers describe come from `s2sdk_sys::generated` instead: an entry carrying `sdk` is never emitted. Never hand-edit; `cargo bridge pins --check` refuses a stale one (`cargo bridge build` runs the check) |
| `../s2sdk/s2sdk-sys/include/s2sdk_gen.h` | **generated** by `crates/s2sdk/s2sdk-sys/gen/gen.py`: the C spelling of the generated SDK numbers (`S2_SLOT_`, `S2_OFF_`, `S2_SIZEOF_`, `S2_VTLEN_`, `S2_CONST_`, `S2_IFACE_`). The bridge itself reads the same IR from `s2sdk_sys::generated`; the header is what a sigs entry's `sdk` expression is evaluated against (`sigcheck.py`, `sdkcheck.py`, `sdkadopt.py`, `cargo bridge pins`) |
| `tests/` | host tests: no game needed (the corpora in `tests/data/` are the retired C's frozen output; a new expected line is written by hand) |
| `PROTOCOL.md` | the JSON-RPC wire contract |

## The commands (`cargo bridge`, crates/cs2-bridge-tools)

| Command | What it does |
|---|---|
| `cargo bridge pins [--check]` | regenerate `src/generated/pins.rs` from the signature database through `sigs/scripts/pinsdata.py` (the one resolver, `sigcheck.py`'s); fails closed on a stale entry, a module whose bytes are not the recorded build, an `expect` that differs from the resolved value, a fingerprint mismatch, or an `sdk` expression that differs from the binary |
| `cargo bridge build` | `pins --check`, then release builds of `cs2-bridge` and `mm-launch` for x86_64-pc-windows-msvc + x86_64-unknown-linux-gnu (memory-bounded: `target/cargo`, `-j2`, an address-space cap, a 900 s deadline), staged as `target/bridge-rs/{win64/{cs2_bridge.dll,cs2mcp_launch.exe,cs2mcp_mmload.dll},linuxsteamrt64/{libcs2_bridge.so,libcs2mcp_mmload.so}}`, each gated by `check-exports` |
| `cargo bridge check-exports [--loader] <file>` | the export gate: the bridge exports exactly `CreateInterface` (a PE also has no unnamed export and imports no CRT DLL); `--loader`: the Metamod loader's `DllMain` / `dlopen`. Reads the PE/ELF tables itself (no llvm tools) |
| `cargo bridge install [--platform wine\|linux] [--dry-run] <install\|uninstall\|status\|ensure>` | install the bridge + the Metamod layout + the launcher into a game tree (below) |
| `cargo bridge runtime <up\|open\|cycle\|status\|logs\|down>` | the podman Wine tools runtime (below) |
| `cargo bridge metamod <image\|linux\|win64\|...>` | build Metamod:Source in a container ([../metamod/README.md](../metamod/README.md)) |
| `cargo bridge testplugin` | build the `crates/mm-testplugin` coexistence fixture into `target/testplugin/` |

`cargo bridge` is an alias (`.cargo/config.toml`) for `cargo run -q -p cs2-bridge-tools --`; it
works from anywhere inside the checkout. The servers run the same binary for their launch-path
`ensure`: `cs2-bridge-tools` next to the server binary (`cargo build --release -p cs2-bridge-tools`),
or the one `--install-tool` / `CS2_MCP_INSTALL_TOOL` names. Tests:
`cargo test -p cs2-bridge-tools` (no game); the pins tests that need the game binaries are
`cargo test -p cs2-bridge-tools --test pins -- --ignored`.

## Signature DB and binaries

The signature database is `../../sigs/` ([../../sigs/README.md](../../sigs/README.md)). The resolver
reads the pinned DLLs from `$CS2_BINS` (default `<repo>/target/game-bins`, gitignored) and the Linux
tree from `target/game-bins-linux-<build>`; fill them once per checkout (sigs/README.md
"Binaries", "Linux binaries"):

```sh
python3 sigs/scripts/stagebins.py --game "${CS2_MCP_PROJECT_ROOT:-.}/workshop/run/app" --out target/game-bins
python3 sigs/scripts/sdkcheck.py         # the generated SDK numbers vs the binaries
cargo bridge build                       # pins check -> build both targets -> export gate
cargo bridge install install             # DLL + sdkenginetools.txt manifest entry + Metamod + launcher
cargo bridge runtime cycle               # up -> open
```

`cargo bridge runtime` and `cargo bridge install` read the project root from
`CS2_MCP_PROJECT_ROOT` (default: the repo's main checkout, so a worktree shares it;
`CS2DOCKER_ROOT` is the deprecated alias) for the `workshop/run/app` tree and `.podman/`, and the
runtime the container image from `--image` / `IMG` / `CS2_MCP_IMAGE` (default
`ghcr.io/personcopperfine/cs2-docker:latest`). Every runtime knob is a flag with the old
environment variable as its fallback (`TOOL`, `ADDON`, `CONTAINER`, `IMG`, `EXTRA_ARGS`,
`POC_ASSET`, `WAIT_*`): `cargo bridge runtime <command> --help`.

## Install/uninstall/status + launch flags

`cargo bridge install` installs the bridge + the Metamod layout + the launcher (`crates/mm-launch`;
Metamod is a hard dependency) into the shared Wine tools install, and `cargo bridge runtime` loads it:

```sh
cargo bridge install install                # tool DLL (ONE image) + manifest entry + Metamod (standard
                                            # addons tree) + cs2mcp_launch.exe/cs2mcp_mmload.dll
cargo bridge runtime up                     # runs `install --platform wine ensure` first; loads cs2_bridge;
                                            # adds -insecure itself (Metamod refuses without it)
cargo bridge install status                 # what is installed (sha256); `uninstall`: restore + remove
```

**Launching.** The Wine sessions start `cs2mcp_launch.exe`, not `cs2.exe`
(`cargo bridge runtime up`: `entrypoint.sh cs2mcp_launch.exe -tools ...`; `docker.rs` `wine_command` /
`wine_client_command` for the client-mcp launches). The launcher starts `cs2.exe` suspended,
injects `cs2mcp_mmload.dll`, and the DLL redirects the one `LoadLibraryExW` of
`...\csgo\bin\win64\server.dll` to the Metamod loader; a failure exits 3 with
`[cs2mcp-launch] FAIL: ...` instead of running without Metamod. The Linux client and dedicated
sessions prepend `csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so` to `LD_PRELOAD` on
the `cs2.sh` command only (`client-mcp/docker/linux-*.sh`) and refuse to launch when it is
missing. **No `gameinfo.gi` edit is needed on either platform**, and a real Windows /
Steam-launched retail CS2 is not a target (containers only). Details, the layouts and the spike
evidence: [../metamod/README.md](../metamod/README.md) "Launching" and "The layout (both platforms)".

`install` gates the DLL with `check-exports` (exactly the `CreateInterface` export) before
touching anything, backs up every pre-existing file to
`<cs2-mcp>/target/bridge/backup-rs-<UTC>/` first, is idempotent (re-runs change nothing), and
`uninstall` restores the oldest backup (the pre-bridge original) (`cmp`-verified), removing only
what install added. The backup and lock formats are the ones the retired shell installer wrote, so
a tree it installed is checked and uninstalled the same way. `ensure` is the launch-path command:
a read-only per-artefact check (`ok` / `missing` / `stale: <why>`, exit 0 + untouched when all
current), otherwise it refuses (exit 4) while any running container mounts the tree or runs
`install` to repair (backup first); "no built DLL and nothing installed" fails `build it first`
(exit 1). `--dry-run` prints everything it would do and changes nothing (`ensure --dry-run` exits
3 when something would be repaired). A tree whose backups record gameinfo-era edits (an install
by the shell installer before the gameinfo-free layout) is refused: that migration is no longer
supported; `git show 8cdea42:bridge/install-rs.sh` is the last version that performed it.

`install`/`uninstall`/`ensure`-repair refuse while ANY running container mounts the tree (wine: a
bind of the app dir or a parent/child of it; a failing podman also refuses, naming the problem).
`--platform linux` targets the cs2-home volume's Linux game dir instead - the STANDARD
`csgo/addons/metamod` layout (client + dedicated; no gameinfo edit; the Linux client lane adds
`-insecure`), run through `podman unshare` with backups under `backup-linux-<UTC>`. `CS2_GAME` /
`CS2_LINUX_GAME` point it at a test tree (layouts: [../metamod/README.md](../metamod/README.md)
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
per-object vptr copy is gone. The wire behaviour is [PROTOCOL.md](PROTOCOL.md) §18-§20.

**Coexistence, live** (verified with the since-retired live test `test_metamod_coexist.py`,
`git show 8cdea42:bridge/test_metamod_coexist.py`: the bridge plus the
`crates/mm-testplugin` fixture on `AllowSaveRestore`, modes `passive`/`supersede0`/`supersede1`,
both hook orders in one session). Passive hooks of both plugins fire on every call and change
nothing. KHook runs the **newest** hook's pre first, and of two equal `Supersede`s the first saved
value wins - so the newer hook's value wins. When the other plugin's value wins, the bridge
counts and logs the conflict (`ahead`). When the bridge's value wins, the other plugin's
`Supersede` was already discarded by KHook before the bridge's post runs and nothing KHook
exposes shows it: **that order counts no conflict** (the other plugin sees the final value in its
own post). After a change to the hook layer, repeat that check: `cargo bridge testplugin` builds
the fixture into `target/testplugin/`, and `CS2MCP_TESTPLUGIN_MODE` / `CS2MCP_TESTPLUGIN_LOG`
(passed into the container by `cargo bridge runtime up`) select its mode and log.

## The Metamod plugin table (`metamod.plugins`, m3.11 lane B4)

`metamod.plugins` is the bridge's first Rust-only method row: it is NOT in the retired C
table (`registry.rs` `METHODS`) but in
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
  the NEGATIVE pins that the method never calls the manager off a callback thread). Wire:
  [PROTOCOL.md](PROTOCOL.md) §23.

## Console variables (`cvar.get`, m3.13)

`cvar.get {names:[..]}` reads cvars from the engine's `ICvar` (`VEngineCvar007`, tier0): `FindConVar` through
the MSVC hidden return pointer, `GetConVarData` with the `ConVarRef` by value, members from the generated
`ConVarData`. The object is held to the pinned `CCvar` vtable (`T0V_CCVAR_VT`, slots `T0VS_CVAR_FIND` /
`T0VS_CVAR_GET_DATA`). Wire contract: [PROTOCOL.md](PROTOCOL.md) §21. Registered in
`RUST_METHODS` (`NEED_TIER0`), body in `ops/cvar.rs`. `console.exec` also accepts `-tools` sessions since m3.13.
The live checklist is `workflows/streamline/FIXME.md` (WP1.3).

## Job inputs (`job.start`, m3.16/m3.17)

`job.start` runs `resourcecompiler` on one content-relative input (`ops/job.rs`: `INPUT_EXTENSIONS`, `PLAIN_FLAGS`).
m3.16 added the map-stage flags (`-world -phys -vis -bakelighting`), m3.17 the `js` extension: a cs_script source
(`maps/scripts/<name>.js`) compiles to its `.vjs_c` (hammer-mcp `cs_script {action:"compile"}` / `compile_asset`).
Wire contract: [PROTOCOL.md](PROTOCOL.md) §14.1. The three allowlists (bridge, hammer-mcp
`jobs.rs`, the mock in `cs2-mcp-common`) stay equal.

## Per-face texture alignment (`mesh.setFaceTexture`, m3.14)

One write method over four hammer.dll pins (`HFN_MESH_{SET,GET,JUSTIFY,ALIGN}_FACE_TEXTURE`): align to the
world grid / the face plane, justify, absolute shift/scale/rotate and (m3.18) absolute axes, per face, one
undo entry. Wire contract: [PROTOCOL.md](PROTOCOL.md) §12.6. Registered in `RUST_METHODS` (`registry.rs`), body
in `ops/mesh.rs`.

## Adding a method (the procedure both servers rely on)

1. A new virtual slot, member offset, struct size or const is a **header declaration first**: add
   the method/field to the s2sdk headers (a fork commit in `crates/s2sdk/upstream`, `unkXYZ` names
   and `AMNOTE:` per its AGENTS.md, no slot numbers in comments), bump `UPSTREAM.lock`, run
   `python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py`. The Rust side spells the generated
   `s2sdk_sys::generated::<module>::<Class>` types, `<Class>_slots::<m>` consts and the generated
   typed vtable fields (`<Class>::vtable(p).<m>`) - never a hand-written number.
2. The **binary measurement second**: reverse the call path in IDA and record it as an entry in
   `../../sigs/signatures/<module>.json` (`../../sigs/ida/ida_sigs.py` generates the locator;
   `sigcheck.py resolve --module <m>` must give the value IDA shows) carrying the `sdk`
   expression, written with `python3 sigs/scripts/sdkadopt.py PIN=S2_EXPR` (it checks the macro
   exists and equals the measurement). `sdkcheck.py` must show the new `sdk:` row as PASS. Only a
   number the SDK cannot give (an RVA, a Qt chain, a literal) gets a pin; then
   `cargo bridge pins` regenerates `src/generated/pins.rs` (commit it). A new module also needs
   its row in `WINDOWS_MODULES` (`crates/cs2-bridge-tools/src/pins/mod.rs`) and in the bridge's
   `MODULES` / `PIN_*`.
3. Add the method's row to `RUST_METHODS` in `registry.rs` (the gate runs before the body),
   implement the handler in `crates/cs2-bridge/src/ops/<group>.rs` and add one line to
   `ops/mod.rs`'s `GROUPS`.
4. Update [PROTOCOL.md](PROTOCOL.md): the method's section, plus §5 for a new error code and
   §6.5 for a new undo label, with `[V live]` evidence from a live session (`cargo bridge runtime
   cycle`, then the server tool). Bump `BRIDGE_BUILD` in `crates/cs2-bridge/src/lib.rs`.
5. Consume the method from a server tool (capability-gated); the servers never see addresses.

Game update: [../../workflows/game-update.md](../../workflows/game-update.md) lane B (`cargo bridge pins` refuses to
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
