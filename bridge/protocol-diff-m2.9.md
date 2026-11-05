# Proposed protocol changes for bridge m2.9 (game state + sound events)

**Applied 2026-10-11** to `../../idabins/docs/` (bridge-protocol.md rev 11 §4/§6/§7/§8 + header, WORKFLOW.md
§11 vtable-wrap rule, ISoundOpSystem.md, client-input-pipeline.md, README.md), with the counted totals
(57 registered / 39 implemented, not the 56/38 first written here). **Not applied:** the signature merge —
by decision the entries stay in `proposed-sigs/` (overlaid by `pins.py`). Kept below as the change record.

Originally: apply to `../../idabins/docs/data/bridge-protocol.md` (not edited from this repo). Evidence and
design: `../client-mcp/research/game-state.md`, `../client-mcp/workflows/11-game-state.md`.

## §7 method table — new rows

| Method | Status | Bridge m2.9 | Params | Result | Call (page) | Notes |
|---|---|---|---|---|---|---|
| `game.local` | ready [V runtime 2026-10-11] | implemented | `{}` | `{tick, inGame, map, playerSlot, viewAngles, player:{…, activeWeapon}}` | engine2 `Source2EngineToClient001` slots 37/39/65; client `CG_LOCAL_CONTROLLERS[0]` cross-checked with slot 34 `GetLocalPlayer`; fields via `SchemaSystem_001` (ISchemaSystem.md recipe) | **No Hammer** (needs client.dll + engine2 + schemasystem pins). Read-only, main thread. `-32024 NoLocalPlayer` when not in a game. [I: not yet run] |
| `game.weapons` | ready [V runtime] | implemented | `{}` | `{tick, inGame, map, active, count, weapons[]}` | `C_BasePlayerPawn::m_pWeaponServices` -> `CPlayer_WeaponServices::m_hMyWeapons`/`m_hActiveWeapon`; `C_BasePlayerWeapon::m_iClip1/2`, `m_pReserveAmmo`; `C_EconItemView::m_iItemDefinitionIndex` | read-only [I] |
| `game.radar` | ready [V runtime, one player] | implemented | `{}` | `{tick, inGame, map, players[], bomb[], hostages[]}` | controllers entindex 1..64 (`cs_player_controller`), identities named `planted_c4`/`weapon_c4`/`hostage_entity`; spotted = `EntitySpottedState_t` | read-only; lists every entity, reports the game's spotted flags [I] |
| `game.schemaProbe` | ready | implemented | `{names?}` | `{results:[{name, found, scopeName, isGlobal}]}` | ISchemaSystem slot 13 + scope slots 28/29 | diagnostics [V runtime] |
| `game.selectWeapon` | ready [V runtime] (slot/inventory commands and weapon/defIndex/handle, 2026-10-11) | implemented | `{slot}` \| `{command}` \| `{weapon}` \| `{defIndex}` \| `{handle}` | `{queued, via, command}` \| `{queued, via, weapon}` | slot/command: engine2 `InputService_001` slot 25 `AddCommandText(5, "slot2"/"invnext", 0)` — the ConCommands client.dll registers (client!0x1800DEDE0). weapon forms: the held weapon (local pawn `m_hMyWeapons`) -> `CCSGOInput` slot 9 `MakeWeaponSelection(this, 0, weapon)` (client!0x180B77040) -> usercmd `weaponselect`; CS2 has no `use <weapon>` command | allow-listed forms only; not gated on `-testscript` [I] |
| `sound.status` / `sound.subscribe` | ready [V runtime: wrap records; listener mode silent] | implemented | `{mode?: wrap\|listener\|both}` | `{installed, mode, pending, total, dropped, ringSize}` | `wrap`: vptr swap of `SoundOpSystem001` (`CSoundOpSystem`) to a vtable copy wrapping slots 11/12/13; `listener`: ISoundOpSystem slot 81 add-listener with a bridge-owned object (slot 5 start callback), slot 82 to remove (ISoundOpSystem.md) | installs on first use; wrap is the default until the listener's entity field is verified; uninstall of both at PreShutdown [I] |
| `sound.drain` | ready (runtime check) | implemented | `{max?, resolve?}` | `{events[], truncated, dropped, total, pending}` | ring buffer; `truncated:true` when the 96 KB reply filled before `max` events: the unwritten events go back to the ring front (oldest go into `dropped` only if the ring is full), so call again; names via `ISoundEventManager` slot 2 `GetSoundEventName(hash)`; origins via the source entity at drain time | [I] |
| `sound.uninstall` | ready | implemented | `{}` | status | restores the vptr | |

## §8 error codes — new

| Code | Name | Meaning |
|---|---|---|
| -32023 | `SchemaUnavailable` | `SchemaSystem_001` has no `client.dll` type scope yet (or a needed class/field is missing); nothing was executed |
| -32024 | `NoLocalPlayer` | no local controller/pawn (not in a game, dead/spectating, weapon services missing); nothing was executed |

## §5/§6 notes
- Pinned modules: add `schemasystem.dll` (`PIN_SCHEMA`) and `soundsystem.dll` (`PIN_SOUND`) to the
  module list; `control.capabilities.modules` now reports 10 modules.
- m2.9 adds the methods above; no major bump (additive).
- Threading paragraph: sound wrappers and the slot-81 listener run on the sound update's thread
  (under the sos lock) and only write a spin-locked ring; all `game.*` reads run in the main-thread drain.
- ISoundOpSystem.md open question "Interface of the slot 81/82 listener objects": resolved —
  6 used slots (0: end of ProcessEventQueue `(int*)`, 1: `(int*)` 0x1801D0D90, 2: `(i64*)` and 4: `(int*)`
  in 0x1801CFC90, 3: lib-stack-field message queue, 5: `(guid, record+0xB4, record+0xB8)` after every
  successful start in 0x1801C5D90 / 0x1801E1F80); returns discarded; no registrant inside soundsystem.dll.

## Other idabins edits to make
- `docs/README.md` "State" row: bridge **m2.9**, 57 methods registered / 39 implemented.
- `docs/WORKFLOW.md` §11 — proposed exception text, to add after "Never patch or detour Valve code
  from the bridge":

  > *Vtable wraps.* When no registration point exists, the bridge may swap one object's vptr to a
  > private copy of its pinned vtable in which a slot points at a wrapper that **records and then calls
  > the original target unchanged** (no instruction is modified, no return value is altered). Rules: the
  > vtable is identity-pinned and its wrapped slots are slot-pinned (`PinSlot`), so a changed slot
  > fails closed; the wrapper is re-entrant, never takes a game lock, never calls back into the wrapped
  > interface, and only writes a bridge-owned spin-locked buffer; the original vptr is restored on
  > `sound.uninstall`/`input.uninstall` and at `PreShutdown`; a supported sink (listener registry,
  > game-event handler) is preferred and the wrap is documented as a fallback. Current wraps: m2.9
  > `sound.*` (`CSoundOpSystem` slots 11/12/13; the slot-81 listener registry is the supported alternative).
- `signatures/{client,engine2,schemasystem,soundsystem}.json`: merge `bridge/proposed-sigs/*.json`
  (entries carry `expect[2000930]`); for the engine2/schemasystem/soundsystem vtables that already
  exist (`CEngineClient` as `EV_ENGINECLIENT_VT`, `CSchemaSystem`, `CSchemaSystemTypeScope`,
  `CSoundOpSystem`, `CSoundOpSystem::col_8`) prefer adding the `bridge` macro to the existing entry
  over the duplicate the proposed file carries, then delete the proposed files.
- `docs/interfaces/ISoundOpSystem.md`: "MCP relevance" — add the wrap and the ring.
