# Protocol changes in m3.11: KHook and Metamod everywhere

Bridge build `m3.11` is m3.10 plus the KHook project (branch `bridge/khook`, design
`target/orch/khook/DESIGN.md`). The bridge is now one Metamod plugin among many, on every launch
path. Its game hooks are KHook hooks that coexist with other plugins' hooks, and on a host
without the tools framework (Wine client, Linux client, Linux dedicated) a KHook on
`ISource2Server::GameFrame` drives it.

This file is the wire and log contract a client, a test or another plugin can rely on. It merges
the lane drafts (B1+B2, B3, B4, E, L, M). Behaviour that the wire cannot see is listed only
where a caller has to know about it.

m3.9's scoped exception ("never KHook CCSGOInput / the save-restore config / CSoundOpSystem") is
**withdrawn**. Both of its reasons are gone:

- the identity check asks KHook for a hooked slot's saved original (`modules::slot_matches`);
- one image serves both roles, so the `IKHook*` lives in the image that runs the method bodies.

## 1. New method: `metamod.plugins`

This is a Rust-only registry row (`registry::RUST_METHODS`) with needs 0, answered on the IPC
lane from a snapshot that is rebuilt on the main thread at Load, AllPluginsLoaded and every
plugin load, unload, pause and unpause event (`ops/metamod.rs`).

```json
{"plugins":[{"id":1,"file":"<path as Metamod has it>","status":"running","source":0,"self":true}, ...],
 "generation":2,"rebuiltAtMs":54}
```

- **`status`**: one of `running`, `paused`, `refused`, `error`, `not_found` or `unknown(<n>)`,
  from Metamod's `Pl_Status`.
- **`self`**: marks the bridge's own row.
- **Without Metamod**: `-32017 {"blockedOn":"metamod: not loaded"}`.
- **Without the plugin manager**: `-32017 {"blockedOn":"metamod: IPluginManager unavailable"}`.
- **Known gap**: our own `meta pause` is not visible until another plugin event arrives, because
  Metamod skips the acting plugin.

## 2. `NEED_KHOOK` and the hook rows

- **The `NEED_KHOOK` bit** (4096, Rust-only) is satisfied when an `IKHook` is installed. It is
  carried by the rows whose body installs a KHook hook: `game.saveRestore`, `input.set` and
  `sound.subscribe`. Without KHook (no Metamod, or after `meta unload`) these rows answer
  `-32017 {"blockedOn":"metamod: KHook not loaded"}`.
- **Off-ramps and report rows never need it:** `input.clear`, `input.status`, `input.uninstall`,
  `sound.status`, `sound.drain`, `sound.uninstall`.
  - `sound.status` and `sound.drain` still arm the wrap on first use while KHook is present,
    which is the C behaviour `drain_sounds` and `installOnly` rely on.
  - Without KHook they skip the wrap half and append `"blockedOn":"metamod: KHook not loaded"`;
    a `{"mode":"listener"|"both"}` request still registers the listener.
- **Capabilities:** the caps mask folds `NEED_KHOOK` in, so a Metamod reload does not change the
  advertised set.
- **Module status (`control.capabilities` and `hello`):** a module entry may carry
  `"relaxed":[...]`, naming the pins the KHook-aware identity check relaxed. The member is
  omitted when the list is empty, and a change bumps the caps revision.

## 3. Conflict counters (DESIGN §13)

Hooks are passive by default and may supersede. When two value changers meet on one call the
bridge does three things:

- logs a rate-limited `khook conflict: <target> <ahead|after>` line;
- adds 1 to the row's `conflicts` counter, which resets at each arm;
- `debug_assert!`s in debug builds only.

| row | new member (appended last) | target |
|---|---|---|
| `game.saveRestore` | `"conflicts":N` | `ISource2ServerConfig::AllowSaveRestore` |
| `input.*` (every reply) | `"conflicts":N` | `CClientInput::unk3023` (view angles) |

The sound start hooks only observe, so they have no counter.

**Live (`bridge/test_metamod_coexist.py`).** KHook runs the newest hook's pre first, and of two
equal-rank `Supersede`s the first saved value wins.

- When the other plugin's value wins, the bridge sees it as `ahead` and counts it.
- When ours wins, the other value was discarded before our post ran, and KHook exposes nothing
  that would show it. **No conflict is counted in that order.** That plugin sees the final value
  in its own post.

## 4. `McpBridge_001` status object: v3

Other plugins reach this object through `OnMetamodQuery`.

- **v2:** `"image"` became `"roles":[...]` (`mm`, `tool`, in that order).
- **v3:** adds the tick source:
  ```
  {"metamod":"present","khook":true,"tick":N,"roles":["mm"],"tickSource":"gameframe",
   "gameframeTicks":N,"gameframe":"installed","gameframeOffThread":false}
  ```
  - `tickSource` is one of `tools`, `gameframe` or `none`.
  - `gameframeOffThread` becomes `true` once a frame has arrived off the Load thread
    (`sv_tick_parallel_with_client`); those frames are counted but never ticked.

## 5. Module identity on a Metamod host (Windows)

Metamod's loader stub is also called `server.dll` (`csgo\addons\metamod\bin\win64\`), and the
loader mapped it first.

- **Before:** `GetModuleHandleA` answered with the stub, so `server.dll` was `"mismatch"` and
  `game.saveRestore` answered -32017.
- **Now:** pinned modules are chosen by full path:
  - an image under a `metamod` directory below the game root never counts;
  - the image under the game root wins;
  - anything still ambiguous is refused and stays `not_loaded`, with one `verify: <name>: ...`
    line.

Lane M moved this selection into `s2sdk::module` so the bridge, `mod_iface` and s2sdk agree.

## 6. Hosts and ticks

| host | roles | tick | bridge RPC |
|---|---|---|---|
| Wine `-tools` | `mm+tool` (one image) | tools `RunFrame` | yes |
| Wine client | `mm` | `GameFrame` KHook | once a map runs (none at the main menu) |
| Linux client | `mm` | `GameFrame` KHook | once a map runs |
| Linux dedicated | `mm` | `GameFrame` KHook | yes; `server_up` passes `+sv_hibernate_when_empty 0` (opt-out `hibernate:true`), because a hibernating server runs no frames |

On a Metamod-only host, `meta unload` stops the listener, the queue and `session.json`. A refused
unload leaves the plugin inert until the unload is retried.

**Linux library mapping:** once it has ticked on the game's main thread, the bridge `.so` stays
mapped after `meta unload`. glibc pins the library through a std thread-local destructor
(`__cxa_thread_atexit_impl`). A later `meta load` works, but it reuses the old image and its
statics (for example, the tick counter continues), so replacing the `.so` needs a server restart.

## 7. Paths (lane L)

- **Wine:** the log is `Z:\app\mcp_bridge_rs.log`, and `session.json` is under the prefix's
  `AppData\Local\cs2mcpbridge\`.
- **Linux:** `CS2_BRIDGE_LOG`, else `$XDG_RUNTIME_DIR/cs2mcpbridge/`, else
  `$HOME/.local/share/cs2mcpbridge/` for both `bridge.log` and `session.json` (one helper,
  `discovery::linux_dir`). The game containers have no `XDG_RUNTIME_DIR`, so they use
  `/home/steam/.local/share/cs2mcpbridge/`. That is the shared `cs2-home` volume, so the client
  and dedicated containers append to the same log.

## 8. Teardown and log lines

`Unload` (mm) and `PreShutdown` (tool) tear down the hook owners in this order: `gameframe`,
`input`, `sound`, `sound listener`, `save_restore`. Then they wait for the verifier,
`remove_all_blocking` (which drains lost removals), and uninstall the KHook adapter. A listener
refusal keeps the API.

New and changed lines:

| line | meaning |
|---|---|
| `[cs2-bridge <roles>]` | log tag; roles is `mm`, `tool` or `mm+tool` |
| `khook: IKHook installed into s2sdk::hook` / `khook: IKHook uninstalled` | |
| `Load: tick installed (ISource2Server::GameFrame -> wiring::tick)` | |
| `gameframe tick N` | logged at powers of two |
| `gameframe: GameFrame off the main thread (...) - not ticking` | logged once |
| `gameframe: tick not installed: <why>` | |
| `metamod: IPluginManager available` / `metamod: plugin table rebuilt` | |
| `hooks teardown: <owner>: <outcome>` | one per owner |
| `input: hook installed (KHook CCSGOInput slots unk3021/unk3023)` | replaces the vptr-copy text |
| `sound: hook installed (KHook CSoundOpSystem StartSoundEvent x3, post)` | replaces the vptr-copy text |
| `test: save/restore unlocked (ISource2ServerConfig::AllowSaveRestore -> 1 via KHook)` | |
| `khook conflict: <target> <ahead\|after>` | |
| `verify: <name>: the only image mapped under this name is Metamod's loader stub; not used` | |
| `verify: <name>: more than one game image is mapped under this name; not used` | |

## 9. Fixed in this build

**`sound.status` / `sound.subscribe` with `{"mode":"listener"|"both"}` crashed the game.** This
predates the KHook project; it was there since the Rust port, and in m3.10. The listener object
handed to `AddSoundEventListener` was the vtable array itself, with no vptr in front of it.
soundsystem's listener loop (`soundsystem.dll+0x1c8849`, `call [rax+0x18]`) then called through
code bytes. Lane M registers an object whose first word points at the table.

## 10. Unchanged

- Every C row's arguments, member order and error texts, except the members appended above.
- `BRIDGE_BUILD`'s shape: `m3.N`.
- `console.exec` is still `-testscript` only. In the parked vtest harness the command buffer is
  held by `Test_WaitForCheckPoint`, so a queued command runs only once the script moves on. Put
  commands in the test script, or release a checkpoint first.
