# Protocol changes in m3.9 — lane HOOKS (`game.saveRestore`, `input.*`, `sound.*`)

Bridge build `m3.9` (Rust bridge) = m3.8 + this group.

Worker brief: M4 lane HOOKS. This file documents the lane's wire surface and every place
the Rust port could not be a byte-level 1:1 of the C (`bridge/mcp_bridge.c`,
`bridge/mcp_bridge_game.inc`). It also carries the M4 hooks design's scoped exception,
which binds every future sweep of this crate:

> **Scoped exception (M4 hooks design, after the Fable consult):** these three rows swap
> ONE object's vptr to a private copy of its vtable — the CCSGOInput (`input.*`), the
> ISource2ServerConfig (+8 view) config object (`game.saveRestore`), and the CSoundOpSystem
> start slots (`sound.*`, wrap) plus a bridge-owned listener object (`sound.*`, listener) —
> exactly as the C does (`mcp_bridge.c:4766-4980`, `:4999-5035`,
> `mcp_bridge_game.inc:891-1100`). **Never KHook CCSGOInput / the save-restore config
> object / CSoundOpSystem from this crate**: KHook patches the game's shared vtable in
> place, which would break the bridge's own pinned-slot identity check (`ops/ident.rs`
> re-reads the pinned slots of exactly those tables — `pins.rs:594-596` slots 30-32 of
> `CV_CSGOINPUT_VT`, `pins.rs:598` slot 21 of `GSV_GAMECONFIG_SERVER_VT`, `pins.rs:605-612`
> the sound slots), and its `IKHook*` lives in the other DLL image.

## Rows moved from "not advertised" to served

The method table is unchanged (no `registry.rs` edit; the rows were already registered with
`implemented: true` and the C's `needs` bits — `registry.rs:128-131, :136, :144-147`). What
changed is that each row now has a body in [`crate::ops`], so `dispatch::is_ported` serves
it and `control.capabilities` advertises it once the needs modules verify:

| Row | needs | body |
|---|---|---|
| `input.set` / `input.clear` / `input.status` / `input.uninstall` | NEED_INPUT (client\|engine) | `ops/input.rs` |
| `game.saveRestore` | NEED_SERVER | `ops/game/save_restore.rs` |
| `sound.status` / `sound.subscribe` / `sound.drain` / `sound.uninstall` | NEED_SOUND | `ops/sound.rs` |

## The wire, arm by arm

| Arm | m2.20 (C) | this build |
|---|---|---|
| `input.*` | 2048-byte result buffer (`mcp_bridge.c:6047`), **no overflow branch** — a too-large status is emitted truncated | identical bytes, same tail (no `-32603` answer, deliberately not `finish`'s ovf branch) |
| `sound.*` | 98304-byte buffer (`mcp_bridge.c:6032`), overflow answered `-32603 "InternalError: sound drain result too large"` (`:6037`) | identical (`finish` with that message) |
| `game.saveRestore` | the test arm's 512-byte buffer (`mcp_bridge.c:5998`), no overflow branch; reply `{"enabled":b}` (`:5059`) | identical; `finish`'s unreachable ovf check kept, as the group's other test-arm rows |

Replies: `input.set` / `input.clear` / `input.status` / `input.uninstall` all answer
`in_status_json`'s object (`{"installed":b,"held":[...],"angles":[p,y,r]|null,"frames":N,
"edges":N,"angleTicks":N,"buttons":[...]}`, `mcp_bridge.c:4898-4928` — member order and the
unlocked-read bytes unchanged; note the C emits `?` for a null name pointer only, an empty
name string stays `""`). `sound.subscribe` is an alias of `sound.status`
(`game.inc:1118`). `sound.drain`'s per-event member order, the `"flags"` wire key (the
voice-layer byte), the slot-11 `params` object (`{"len":N,"hex":"...","truncated":b}` or
`null`, absent for via 12/13/81) and the slot-81 `aux`/`inst150` members are the C's
(`game.inc:1141-1203`), including the whole-or-rollback rule and the front-requeue of the
unwritten tail.

Errors: `-32017 PrerequisiteFailed` for the identity checks (`g_pInput is not the pinned
CCSGOInput (vtable)`, `Source2ServerConfig001 is not the pinned CCSGameConfiguration
(+8 view)`, `SoundOpSystem001 is not the pinned CSoundOpSystem`), `-32602 InvalidParams`
for `angles must be [pitch,yaw,roll] or null` and `mode must be wrap|listener|both` and
`unknown button or bad list: <name|?>` (the reason built in the C's 96-byte scratch,
`mcp_bridge.c:4960-4964`). `console.exec`'s `-32011` testscript gate is the game lane's,
unchanged.

## Places the port deliberately differs from the C (no byte on the wire for any readable path)

1. **Spinlocks → `Mutex`.** The C's `g_inLock` / `g_sndLock` compare-exchange spinlocks
   (`mcp_bridge.c:4773-4774`, `game.inc:916-917`) become `std::sync::Mutex` around the same
   critical sections. Where the C *reads* the protected state without the lock
   (`in_status_json`, `snd_status_json`, the angle override's `g_inAnglesOn` check), the
   Rust reader takes the lock (or reads the counter atomics) — a data-race fix; the bytes
   for any interleaving are the same, and the multi-field consistency the C's writers
   guarantee is preserved.
2. **`GetTickCount64()` → `wiring::mono_ms()`.** Monotonic milliseconds since this
   process's first call instead of since boot. No reply shows an absolute ms value:
   `sound.drain` shows only `ageMs` differences, so every readable answer is identical.
3. **Bounded copies of game strings.** The C hands raw `const char *` straight to
   `sb_addjson`, reading until NUL (button names, `:4908`; listener names, `:1060`). The
   Rust copies into a bounded 256-byte local first (the pointer passed `mem_ok`); identical
   bytes for every real engine name, a cut instead of a fault for garbage.
4. **The listener vtable is `max(VTLEN, 16)` entries** (review round 2): the generated
   `ISoundEventListener_slots::VTLEN` (6) real slots — covering the slots the game calls
   (0..5, `game.inc:1034-1040`) — padded to the C's 16 (`game.inc:1042`'s
   `#define SND_LISTENER_SLOTS 16`, the shape (b) not in the sigs DB) with the no-op in
   every padding slot at or past `VTLEN`, as the C's `:1078` fill leaves them; a game
   update that adds a virtual reads inside the table.
   The per-slot noops are typed to each slot's real signature where the C cast one
   0-argument function into every slot (same no-op behaviour).
5. **`Sb` gained two additive methods** (`crates/cs2-bridge/src/entity_json.rs`):
   `cap()` (the C's `out->cap`, read by the drain's rollback rule) and `rewind(len)` (the
   C's `out->len = mark; out->buf[mark] = 0; out->ovf = 0;`, `game.inc:1185` — unlike
   `truncate`, the overflow flag is cleared, because the rolled-back event's partial write
   is what set it and the drain keeps writing the tail). No existing caller changes
   behaviour; `truncate` is untouched for its callers.
6. **The Itanium (Linux) arms are compile-only.** The hooks install only after the
   Windows pins verify (`soundsystem.dll` / `client.dll` / `server.dll` are Windows
   images); the Linux dedicated server's `sound.*` / `input.*` rows fail at the identity
   gate exactly as the C's would with zero verified modules. The Itanium thunks carry the
   ABI's own shapes (2-word vtable prefix, no hidden sret) so the type is ABI-correct there
   too.
7. **`game.selectWeapon`'s second arm restored** (`game.inc:869`): the weapon path's
   identity check now also accepts `in == g_inObj && in_installed()` via
   `ops::input::object()` / `ops::input::installed()` (protocol-diff-m3-game.md's left-out
   arm).
8. **Review round 2 fixes (wire-identical, toward the C):** `in_each_name`'s
   `params[start] != b'['` became `params.get(start) != Some(&b'[')`
   (`mcp_bridge.c:4875` — the C reads the NUL when the value is missing, `{"press":`, and
   answers the same `-32602 "unknown button or bad list: ?"`; the Rust indexed past the
   slice and the panic would have answered `-32603`); `sound.drain`'s age is
   `now.wrapping_sub(ev.ms)` (the C's unsigned `now - ev->ms`, `game.inc:1149`);
   `VtSwap::install` writes every copy word through the raw pointer with
   `write_volatile`, never a `&mut` into the copy (no aliasing violation for a reader
   still in the copy across an uninstall / re-install; the wire and the store order are
   unchanged).

## Live parity coverage (the orchestrator runs these)

- `bridge/test_sound_params.py` — `sound.status` install state, `sound.drain`'s via-11
  `params` walk (live mode) and the committed source shape (`--static`, which greps the
  FROZEN C text of `mcp_bridge_game.inc`; checking the Rust instead needs the same
  assertions pointed at `crates/cs2-bridge/src/ops/sound.rs`: the two pins read through
  `offset_of!`, the clamp `!(0..=65535)`, the writer inside the
  `START_WITH_PARAMS_SLOT` guard, `SndEvt`'s fields, `SNC_SOS_VT_SLOTS` as the copy length
  and the listener registration through the typed slot-81 field).
- `bridge/test_client.py` — the game parts (`game.tick`'s `frames`, `game.local`'s
  `inputOverride` while the override is set, `console.exec`'s testscript gate).
- Input parity via a probe (the orchestrator's client-container step): `input.set` press /
  release / tap replies and the counters; `game.selectWeapon` (the command path and, while
  the input hook is installed, the weapon path's second arm).

## Protocol version

This lane does not bump `BRIDGE_BUILD`; the merge brings the other lanes' bump (`m3.8`
after the mesh and render merges) and the orchestrator bumps the minor again when this
lane's merge lands. No blocked row moved in `registry.rs`. The "not ported" examples the
other lanes' tests used (`input.set` was the wave-3 example; `mesh.create` the one before
it) became served rows as their lanes merged: the merged
`src/dispatch.rs`'s `an_unported_row_is_not_ported_even_with_every_module_verified` now
drives the arm with a synthetic `is_ported` closure over a real C-implemented row name, so
it does not depend on which lanes are merged, and the capabilities tests' absent lists keep
only `edit.batch` (the EDIT lane is still unmerged).
