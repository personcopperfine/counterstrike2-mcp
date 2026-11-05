# Protocol changes in m3.1: the M4 GAME group (the Rust bridge answers the wire)

Bridge build `m3.1` (Rust bridge) = m3.0 + this group.

The lane ports the C bridge's game-state and engine ops to the Rust bridge: **`test.checkpoint`,
`test.active`, `console.exec`, `game.tick`, `game.local`, `game.weapons`, `game.radar`,
`game.rules`, `game.entities`, `game.selectWeapon`, `game.schemaProbe`** (11 rows; NOT
`game.saveRestore` - the C swaps an `ISource2ServerConfig` vtable pointer, which is a hook in the
Rust bridge and goes to the hooks lane, so the row stays "not ported" there).

No method was added or removed and no method changed parameters. The replies are ported 1:1 -
same checks in the same order, same error codes/names/`reason` strings, same JSON member order,
same number formatting (`Sb`/`addf`), same buffer sizes and overflow messages - so that the
observable wire behavior of each method is the C's. This file lists every difference from the C
the port could not avoid, and the parity coverage.

## The wire, per method

| Method | C function | Rust body | Wire |
|---|---|---|---|
| `test.checkpoint` | `op_test` (`mcp_bridge.c:5081-5097`) | `ops::game::test_checkpoint` | unchanged (`-32602` / `-32017` / `{"active":b}`) |
| `test.active` | `op_test` | `ops::game::test_active` | unchanged |
| `console.exec` | `op_test` (`mcp_bridge.c:5062-5080`) | `ops::game::console_exec` | unchanged (`-32011` without `-testscript`) |
| `game.tick` | `op_test` (`mcp_bridge.c:5043-5054`) | `ops::game::game_tick` | unchanged (`{"tick":N,"frames":N}`) |
| `game.local` | `op_game_local` (`mcp_bridge_game.inc:487-516`) | `ops::game::game_local` | unchanged |
| `game.weapons` | `op_game_weapons` (`:519-555`) | `ops::game::game_weapons` | unchanged |
| `game.radar` | `op_game_radar` (`:558-644`) | `ops::game::game_radar` | unchanged (incl. `hostagesTruncated`) |
| `game.selectWeapon` | `op_game_select_weapon` (`:834-889`) | `ops::game::game_select_weapon` | unchanged (slot/command/weapon/defIndex/handle) |
| `game.schemaProbe` | `op_game_schema_probe` (`:1210-1243`) | `ops::game::game_schema_probe` | unchanged |
| `game.rules` | `op_game_rules` (`:671-703`) | `ops::game::game_rules` | unchanged (incl. `schemaMissing`) |
| `game.entities` | `op_game_entities` (`:712-770`) | `ops::game::game_entities` | unchanged (incl. the 64-byte trailer rollback and `count`/`truncated`) |
| `game.saveRestore` | `op_test` (`mcp_bridge.c:5055-5061`) | **not ported** (hooks lane) | `-32011 NotImplemented`, `data.blockedOn = "rust bridge m3.0: not ported yet"` |

The two dispatch arms are the C's: the test/console/tick arm over a 512-byte result buffer
(`mcp_bridge.c:5998`) and the game-state arm over a 98304-byte one (`:6012`) with the overflow
answer `InternalError: game state result too large` (`:6018`).

## Differences from the C (none changes a byte on the wire today)

1. **`console.exec`'s command line source** (`mcp_bridge.c:4988-4996`). The C matches
   `-testscript` against `GetCommandLineA()` (one NUL-terminated, space-separated string; the
   scan stops at its NUL). The Rust port does exactly that on Windows: `#[cfg(windows)]` reads
   `GetCommandLineA()` (windows-sys `Win32_System_Environment`) and runs the same matcher over
   the same string. `#[cfg(unix)]` runs the same matcher over `/proc/self/cmdline`
   (NUL-separated argv), where the scan continues past NULs to cover every argument - the data
   source a Linux-dedicated-server build of the C would have needed; the C itself has no such
   build. The match rule per position is the C's byte-for-byte (`:4991-4994`): flag bytes equal
   from the position, then a terminator byte (NUL, space, quote) or end of buffer.

2. **`game.selectWeapon`'s input-hook arm** (`mcp_bridge_game.inc:868-872`). The C accepts
   `g_pInput` when `obj_is` passes **or** `in == g_inObj && in_installed()` (the input hook
   swaps the object's vptr to a private copy). The Rust build has no input hook yet (the
   hooks lane ports it), so the fallback arm has nothing to consult and the condition is
   exactly the `obj_is` arm - which is what the C evaluates while its hook is not installed.
   When the hooks lane lands, its CCSGOInput hook state must be added to this condition (one
   place: `crates/cs2-bridge/src/ops/game/mod.rs`, `op_game_select_weapon`).

3. **`game.tick`'s `frames` and `game.local`'s `inputOverride`** read the C's input-hook
   statics (`g_inAnglesOn`, `g_inAngles`, `g_inFrames`, `mcp_bridge.c:4769-4771`). The
   Rust bridge defines them as the public atomics `ops::game::{IN_FRAMES, IN_ANGLES_ON,
   IN_ANGLES}`; the input hook that writes them is not ported yet, so they read 0/false/null -
   exactly what the C reads while its hook is not installed. The hooks lane should write these
   (or hand the group its own statics to read) when it lands.

4. **The C's 512-byte arm has no overflow check; the shared `ops::finish` does**
   (`mcp_bridge.c:5996-6008` vs the Rust's one tail; the game-state arm's check is `:6017-6018`).
   If a test/console/tick result ever overflowed its 512 bytes, the C would emit the truncated
   JSON, the Rust bridge would answer `-32603`. Unreachable today: the arm's results are
   `{"tick":N,"frames":N}`, `{"active":b}`, `{"queued":true}` - all far below 512 bytes.

5. **Name copies instead of in-place pointers.** The C copies game-owned name strings
   (`gs_copy_cstr`, `mcp_bridge_game.inc:249-260`, bounded at 256 bytes, one `mem_ok` per 4 KiB
   page) into a 4-slot ring and hands out pointers; the Rust readers hand out the same bounded,
   page-checked copies by value. The C's `gs_cstr` (the player name, `:202-207`) and `addjson`
   of a scope name also read in place after one probe; the Rust copies them through the same
   page-checked reader. The JSON output is identical (both stop at the NUL); only pointer
   lifetimes differ.

6. **`GetLocalPlayer`'s call shape** (`mcp_bridge_game.inc:299`, the C's own typedef comment).
   The C calls the real Windows slot as `(this, int *out, int slot)` - the MSVC hidden-return
   pointer for the class return type `CPlayerSlot` - and that same comment prescribes
   `int(this, slot)` for a Linux port. The port does both: the generated Rust vtable field has
   exactly these shapes per target (`s2sdk-sys/src/generated/engine.rs`, the `cfg(windows)` /
   `cfg(not(windows))` vtable structs; the slot is 34 on both), so `gs_local_controller` calls
   the typed field under each cfg - the Windows hidden-return pointer with a `CPlayerSlot`
   out-slot seeded -1 (the C's `int ps = -1`, `:314`), and the by-value form elsewhere. No raw
   fn pointer, and no IR disagreement: the generator emits both shapes from the same IR.

## Parity coverage

Host tests (no game; the acceptance gate's paste-outs):

- `crates/cs2-bridge/src/ops/game/tests.rs` - the pure result builders byte-for-byte against
  the C's `sb_add` sequences (weapon, player radar/local shapes, session header, planted/
  carried bomb, hostage, entity, rules tail with `schemaMissing`, probe rows, selectWeapon
  envelopes, tick/active/queued, `addf3`, both F2 layout-mismatch messages), the `-testscript`
  matcher, `jstr`, `gs_is_weapon_name`, `game.entities`' param bounds, the `console.exec` /
  `test.checkpoint` malformed-params paths, the probe's `names`-source selection (array /
  absent / params ending at `"names":`), `gs_layout_check`'s decision/sticky/rollback logic
  (`layout_entry`, `layout_decide`, and the real `gs_layout_check` through the state machine:
  first run -> state 3, re-run -> pass, fabricated mismatch -> sticky refusal with the stored
  message), and `game.entities`' scratch-commit boundary (an entity exactly at / one byte past
  the +64 trailer reserve).
- `crates/cs2-bridge/tests/ops_game.rs` - the HANDLERS-to-C-table self-test, the mandatory
  negative (unverified modules -> `-32017` naming the gate module, exact gate bytes, no body),
  the malformed-params paths through the real handlers, the body-level identity refusals with
  the C's messages, and `console.exec`'s `-32011` outside `-testscript`.

Live parity (the orchestrator runs these under Wine against the Rust bridge; no
`bridge/test_*.py` script drives this group today):

- `client-mcp/research/game_state_probe.py` - drives `game.local`, `game.weapons`,
  `game.radar`, `game.selectWeapon` (plus `sound.*`) against a live session and checks the
  result shapes (map/tick, players, weapons).
- The `game.*`/`test.*` tools of `client-mcp` (`client-mcp/src/game_state_tools.rs` et al.,
  exercised by `client-mcp/tests/game_*_tools.rs`) call the same methods end-to-end.
- `console.exec`, `test.checkpoint`/`test.active`, `game.tick`, `game.rules`,
  `game.schemaProbe` have no dedicated script; `game.tick`/`test.checkpoint` are exercised by
  the vtest runner flows (`worknotes/vtest-scratch.md`), `game.schemaProbe` by the schema
  research probe (`client-mcp/research/`, `docs/interfaces/ISchemaSystem.md`).

`--static` mode: `bridge/test_sound_params.py --static` greps the C source
(`mcp_bridge_game.inc`) for its sound checks only - it never reads this group. A static mode
for this group would need to check, over `crates/cs2-bridge/src/ops/game/*.rs` instead of the
C text: the eleven names in `ops::game::HANDLERS` (and `game.saveRestore` absent), the two
buffer sizes (512 / 98304) and the overflow message, the error strings of every refusal
(`console.exec is only enabled in -testscript sessions`, the three `-32017` object messages,
the `-32024`/`-32025` reasons, the `-32602` reasons quoted above), and the JSON member orders
the unit tests pin. It would also need a way to run `cargo test -p cs2-bridge --test ops_game`
(the checks above are Rust tests, not greps); no script does that today.
