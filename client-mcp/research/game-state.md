# Research: in-game state via the bridge (radar, local view, ammo, weapons, sounds)

Date: 2026-10-11. Binaries: CS2 **2000930** (Windows: `../../../idabins/windows/game/csgo/bin/win64/client.dll`
sha256:44af2aa0d2c64f90, `engine2.dll` 4b8fe29ecdef4f49, `schemasystem.dll` f362c13aada09ee0,
`soundsystem.dll` 4517ee246256f972). Tags per `../../../idabins/docs/FORMAT.md`:
`[V]` read in IDA, `[S]` s2sdk header, `[D]` dump/GameTracking, `[I]` inferred, `[?]` unknown.
**Nothing on this page has been run against a live game yet** (see "Verification").

## Decision: offsets come from the game, locators from the sig DB

Every entity field is resolved **at runtime** through `SchemaSystem_001` (class + field name ->
offset), the same walk `schema_detailed_class_layout` performs
(`../../../idabins/docs/interfaces/ISchemaSystem.md` "How to resolve a field offset at runtime",
all steps `[V]`): `CreateInterface("SchemaSystem_001")` -> slot 13 `FindTypeScopeForModule("client.dll")`
(global scope on a miss -> check scope slot 29 `IsGlobalScope`) -> scope slot 25
`FindRawClassBinding(name)` -> `CSchemaClassInfo` (`+0x08` name, `+0x20` size, `+0x24` field count,
`+0x29` base count, `+0x30` fields x 0x20 {name, type, +0x10 offset}, `+0x38` bases x 0x10 {u32 offset,
class}). Inherited fields are not copied into `m_pFields`: the bridge recurses through the bases and
adds the base offset (`bridge/mcp_bridge_game.inc` `gs_field_in`). A field that disappears in an
update makes the read fail loudly (`null`/error), never silently read garbage.

What is pinned by signature instead (fail closed, `bridge/proposed-sigs/*.json`, all 19 entries
resolve uniquely on 2000930 with `sigcheck.py resolve --strict`):

| Macro | Module | What | Evidence |
|---|---|---|---|
| `CG_ENTITY_CHUNKS` | client | pointer to the `CEntityIdentity` chunk-pointer array | `[V]` client!0x18020BF44 / 0x18016AD41 / 0x18016B110: `*(QWORD*)(chunks + 8*((h & 0x7FFF) >> 9)) + 112*(h & 0x1FF)`; sig site client!0x18015CE1E |
| `CG_LOCAL_CONTROLLERS` | client | `CBasePlayerController*[splitSlot]` | `[V]` client!0x180932F20: after `IVEngineClient2` slot 34 `GetLocalPlayer(&out, slot)` == `entindex - 1`, stores `qword_1825338A8[slot] = this` |
| ~~`CG_VIEWANGLES`~~ | client | **withdrawn**: `xmmword_182573058` is not a standalone static: it lies inside the static `CCSGOInput` object (+0xBF8 of 0xC10, the sub-object `ISource2Client` slot 188 forwards to), written only by the static initializer (client!0x1800EE5A4/0x1800EE5F1) — it read `[0,0,0]` live. View angles come from the pawn's `m_angEyeAngles` (follows the input hook override, `[V runtime]`) |
| `CG_GAME_ENTITY_SYSTEM` | client | `CGameEntitySystem*` | `[V]` client!0x180A16C3B; diagnostics only |
| `EVS_EC_GET_LOCAL_PLAYER` (34), `EVS_EC_IS_IN_GAME` (39), `EVS_EC_GET_LEVEL_NAME_SHORT` (65) | engine2 `CEngineClient` | local player slot, in-game, map name | `[V]` `docs/interfaces/IVEngineClient2.md` |
| `SSV_SCHEMASYSTEM_VT`, `SSVS_FIND_TYPE_SCOPE` (13), `SSV_TYPESCOPE_VT`, `SSVS_TS_FIND_RAW_CLASS` (25), `SSVS_TS_IS_GLOBAL_SCOPE` (29) | schemasystem | the resolver | `[V]` ISchemaSystem.md / CSchemaSystemTypeScope.md |
| `SNV_SOUNDOPSYSTEM_VT`, `SNV_SOUNDEVENTMGR_VT` (+8 base), `SNVS_SOS_START_WITH_PARAMS` (11), `SNVS_SOS_START_BY_NAME` (12), `SNVS_SOS_START` (13), `SNVS_SEM_GET_EVENT_NAME` (2) | soundsystem | sound-event start + name lookup | `[V]` `docs/interfaces/ISoundOpSystem.md` |

Entity identity layout used: `m_pInstance` at `+0` `[S]` (s2sdk `entitysystem.h`), full `CHandle`
at `+0x10` `[S][V]` (client!0x180932F20 reads `identity+0x10` as the handle), `m_designerName`
via schema `CEntityIdentity::m_designerName` (dump: +32 `[D]`). Identity size: schema
`CEntityIdentity` `m_nSize` (112 in this build `[V]` from the `112*` multiplier; s2sdk's struct is
120, so the size is **read from the schema**, never assumed). Chunks hold 512 identities
(`>> 9`, `& 0x1FF`) `[V]`. `CEntityInstance::m_pEntity` (identity pointer) at `+0x10` `[S]`.

## Where each datum comes from

| Datum | Source (class::field, schema) | Notes |
|---|---|---|
| local controller | `CG_LOCAL_CONTROLLERS[0]`, cross-checked with engine slot 34 | `NoLocalPlayer` when absent |
| local pawn | `CCSPlayerController::m_hPlayerPawn` -> handle -> identity | dead/spectating -> no pawn |
| view angles | pawn `C_CSPlayerPawn::m_angEyeAngles` (+ `inputOverride` = the hook's active override) | `[V runtime]`: spawn yaw −157.5°, 90° after `input.set {angles:[0,90,0]}` |
| eye angles (networked) | `C_CSPlayerPawn::m_angEyeAngles` | per pawn, also for other players |
| origin | `C_BaseEntity::m_pGameSceneNode` -> `CGameSceneNode::m_vecAbsOrigin` | |
| eye position | origin + `C_BaseModelEntity::m_vecViewOffset` | |
| velocity | `C_BaseEntity::m_vecAbsVelocity` | |
| health / alive / team / name | `CCSPlayerController::m_iPawnHealth`, `m_bPawnIsAlive`, `C_BaseEntity::m_iTeamNum` (on the controller), `CBasePlayerController::m_iszPlayerName` (inline char[]) | |
| flags / lifeState / scoped | `C_BaseEntity::m_fFlags`, `m_lifeState`, `C_CSPlayerPawn::m_bIsScoped` | |
| spotted (radar visibility) | `C_CSPlayerPawn::m_entitySpottedState` -> `EntitySpottedState_t::m_bSpotted`, `m_bSpottedByMask` | the game's own radar predicate; the bridge lists every pawn regardless and reports the flags |
| active weapon | `C_BasePlayerPawn::m_pWeaponServices` -> `CPlayer_WeaponServices::m_hActiveWeapon` | `m_pClippingWeapon` is not in this build's dump `[D]` |
| inventory | `CPlayer_WeaponServices::m_hMyWeapons` (`CUtlVector<CHandle>` = `{int size; +8 T*}` `[S]`) | |
| ammo | `C_BasePlayerWeapon::m_iClip1`, `m_iClip2`, `m_pReserveAmmo[0]` | |
| weapon identity | `CEntityIdentity::m_designerName` (`weapon_ak47`), `C_EconEntity::m_AttributeManager` -> `C_AttributeContainer::m_Item` -> `C_EconItemView::m_iItemDefinitionIndex` (u16) | gear slot needs the weapon's vdata (`CCSWeaponBaseVData::m_GearSlot` via subclass) `[?]` not read |
| bomb | identities named `planted_c4` (`C_PlantedC4::m_bBombTicking`, `m_bBombDefused`, `m_nBombSite`, `m_flC4Blow`) and `weapon_c4` (`C_BaseEntity::m_hOwnerEntity` -> carried vs dropped) | |
| hostages | identities named `hostage_entity` (`C_Hostage::m_nHostageState`, `m_isRescued`, `m_leader`, `C_BaseEntity::m_iHealth`) | |
| map / tick / in-game | engine slots 65 / `EVS_EC_GET_TICK` (37) / 39 | |
| radar overview | file `resource/overviews/<map>.txt` (`pos_x`, `pos_y`, `scale`, 1024 px image) `[D]` GameTracking | projection in Rust: `px = (x - pos_x)/scale`, `py = (pos_y - y)/scale` |

All field names were checked against this build's schema dump (`../../../idabins/cs2-dumper/output/client_dll.json`
`[D]`; classes `C_CSPlayerPawn`, `C_BasePlayerPawn`, `C_BaseEntity`, `CGameSceneNode`, `C_BaseModelEntity`,
`CPlayer_WeaponServices`, `C_BasePlayerWeapon`, `C_EconEntity`, `C_AttributeContainer`, `C_EconItemView`,
`CCSPlayerController`, `CBasePlayerController`, `C_PlantedC4`, `C_Hostage`, `EntitySpottedState_t`,
`C_CSWeaponBase`, `CEntityIdentity`). cs2-dumper's `offsets.json` is build 14189 (one build older), its
byte patterns were re-found in the 2000930 IDB (`dwLocalPlayerController` -> client!0x180932FC0,
`dwGameEntitySystem` -> 0x180A16C3B, `dwCSGOInput`/`dwViewAngles` -> 0x1800EE59A/0x1800EE5A4) and
the sig entries were generated with the repo's `ida_sigs.py` (stricter uniqueness than cs2-dumper's).

## Weapon selection (native path)

`slot0`…`slot13`, `invnext`, `invprev`, `invnextselect`, `invprevselect`, `lastinv` are **ConCommands
registered by client.dll** (`[V]` client!0x1800DEDE0 registers `"slot1"` with callback
client!0x180C1DFE0, which forwards to the split-screen slot object `qword_182561218[slot]` vfn 79; the full
list is every `sub_1819C2590` ConCommand-ctor call with those names). A key bind queues exactly these
strings on the engine command buffer, so `game.selectWeapon {slot|command}` does the same through
`InputService_001` slot `AddCommandText(5, cmd, 0)` (the primitive `console.exec` already uses
`[V runtime]`) with an allow-list. It is therefore usable outside `-testscript` sessions.
`invnextgun` is only a UI string (client!0x180EA3B28, `CUtlString::Set`), and `invnextitem` /
`invnextnongrenade` do not exist (`[V]` 2000930), so they are not allow-listed.

**There is no `use <weapon>` command in CS2** (`[V]` 2000930: no data reference to a `"use"` string in
client.dll or server.dll, tail-merged strings included; server's client-command handler
server!0x1801C26D0 knows `buy`/`autobuy`/`rebuy`/`open_buymenu`/`pickup_groundweapon` only). That is why
the live `use weapon_knife` probe did nothing. Selecting a *specific* weapon therefore uses the path the
`slotN` commands end in:

- `CCSGOInput` / `CClientInput` vtable **slot 9** = `MakeWeaponSelection(this, splitSlot,
  C_BasePlayerWeapon*)` (`[V]` client!0x180B77040, both `??_7CCSGOInput@@6B@` client!0x181C97B28 and
  `??_7CClientInput@@6B@` slot 9). For split slot 0 it stores the weapon's entity handle (identity+0x10,
  serial-adjusted) at `this+0x694` (= tick accumulator `this+0x228` + 0x46C, right after the accumulator's
  view angles at +0x460) and the command number at `this+0x698`.
- Per-tick CreateMove (`[V]` client!0x180B64E60, `v7 = this + 0x228`) reads accumulator+0x46C; when it is
  not -1 it resolves the handle through the entity chunks and writes `CBaseUserCmdPB.weaponselect`
  (base+104, has-bit 0x400). client!0x180B6BA00 / 0x180B5BA40 reset it to -1.

`game.selectWeapon {weapon:"weapon_x" | defIndex:n | handle:h}` finds the weapon in the local pawn's
`CPlayer_WeaponServices::m_hMyWeapons` (exact designer name; `weapon_knife` falls back to any
`weapon_knife*` / `weapon_bayonet`), then calls slot 9 through the pinned original vtable
(`CV_CSGOINPUT_VT` + `CVS_INPUT_MAKE_WEAPON_SELECTION`, works with or without the input hook's vptr swap)
on the main thread. `defIndex` disambiguates weapons whose entity class is shared.
`[V runtime]` 2026-10-11 (`research/evidence/2026-10-11-game-state-probe-weaponselect.json`, 39/41):
`{weapon:"weapon_knife"}` -> knife, `{defIndex:32}` -> `weapon_hkp2000`, `{weapon:"weapon_ak47"}` (not
held) -> `-32602`; `slot3`/`slot2` still switch knife <-> pistol.

## Sound events

The client handles `GE_SosStartSoundEvent` in client!0x1803AE8B0 (`[V]`; debug string
`"%s:  %f tick(%d) curtime(%f) DoStartSoundEvent(%s)\n"`): it resolves the name with
`ISoundEventManager` slot 2 `GetSoundEventName(hash)` (`(*(g_pSoundOpSystem+8))+0x10`) and starts the
event with **`ISoundOpSystem` slot 11 `StartSoundEventWithParams(out, SosStartParams_t*)`**
(`vfn+0x58`). Local/UI sounds use slots 12/13 (by name). `SosStartParams_t` `[V]` (ISoundOpSystem.md):
`+0x10` hash, `+0x14` source entity index, `+0x18` guid, `+0x22/+0x23` flags, `+0x08/+0x1C` packed
field data (count); **no position field** — position comes from the source entity (the bridge resolves
the entity's origin at drain time) or from packed fields (`[?]` layout not parsed).

### Sink survey (2026-10-11, soundsystem.dll.i64 / engine2 docs)

| Candidate | Exists? | Gives | Thread | Registrable without patching? |
|---|---|---|---|---|
| (a) **`ISoundOpSystem` slot 81/82 add/remove sound-event listener** | yes `[V]` soundsystem!0x1801DE7C0 -> 0x1801C8C20 appends the object to the vector `sos+0x2518` under the sos lock; 0x1801DE6C0 removes | listener vtable: slot 0 `(self, int* processed)` end of `ProcessEventQueue` (0x1801C7AC0); slot 1 `(self, int*)` 0x1801D0D90; slot 2 `(self, i64*)` + slot 4 `(self, int*)` in the per-frame update 0x1801CFC90; slot 3 `(self, msgQueue*)` lib-stack-field queue 0x1801C87A0; **slot 5 `(self, guid, record+0xB4, record+0xB8)` after every successful `StartSoundEvent_Internal`** (0x1801C5D90, 0x1801E1F80). Every call discards the return. **No hash/entity/position in the callback**: name via slot 68 `GetSoundEventNameByGuid` while active; `+0xB4/+0xB8` are copied to instance `+0x1FC/+0x204` (meaning `[?]`); slot 86 instance int `+0x150` is a *candidate* source entity `[?]` | whichever thread runs `CSoundSystem::Update` -> `S_UpdateVoicesPerFrame` (inline on the caller or a `g_pThreadPool` job when `bAsync`) `[V]` ISoundSystem.md slot 22; sos lock held during the callback | **yes**: our own object, no vptr swap |
| (a') audio device / `startmovie` wav capture | the movie recorder registers an audio-recording callback (`CMovieRecorder+16` vtable, `worknotes/movie-scratch.md`) | mixed PCM, not events | mixer | n/a for events |
| (b) `IGameEventSystem` (`GameEventSystemClientV001`) slot 12 `RegisterGameEventHandlerAbstract(CUtlSlot*, CUtlAbstractDelegate&, INetworkMessageInternal*, priority)` for `CMsgSosStartSoundEvent` | yes `[V]` IGameEventSystem.md; message object via `INetworkMessages` slot 13 `FindNetworkMessage("CMsgSosStartSoundEvent")` | the server-sent message: hash, source entity, seed, packed params, start time (`[S]` gameevents.proto) — **only networked sounds**, not client-local ones | main thread (`ProcessQueuedEvents`) | yes, but needs `CUtlAbstractDelegate`/`CUtlSlot` layout `[S]` — not implemented |
| (c) game events `player_footstep`, `weapon_fire`, `player_sound` … | engine game-event listeners exist (same slot 12) | coarse, gameplay-level; not the sound stream | main | yes; not implemented |
| (d) `snd_sos_show_soundevent_start` (+ ~40 `snd_sos_show_*`/`snd_sos_print_*` convars) `[V]` strings soundsystem!0x1804A95E0 | console text per start | name only, via `console.log` | n/a | yes (`console_exec`), lossy/slow |

**Decision**: the slot-81 listener is the supported, patch-free sink and is implemented as
`sound.subscribe {mode:"listener"}` (`via: 81` records with `name`, `guid`, `aux`, `inst150`). Because
its callback does not carry the source entity, the **wrap stays the default** (`mode:"wrap"`, hash +
entity + name) until a live run shows whether slot 86's `+0x150` is the entity index; `mode:"both"`
runs them together for that comparison. Candidate (b) is the clean source for *networked* sounds with
positions in packed params and is the recommended follow-up if the listener's entity proves unusable.

Wrap (default) — safety rules: the bridge swaps the `CSoundOpSystem` singleton's vptr to a private copy of its 103-slot
vtable in which slots 11/12/13 are **wrappers that record and then call the original** (first
vtable *wrap* in the bridge; the input hook only filled empty slots). Records go into a 256-entry
spin-locked ring (`seq, hash, name, entindex, guid, via, flags, ms`); `sound.drain` returns and
clears them, `dropped` counts overflow. Which thread calls slot 11 is `[?]` (client main thread
assumed for the game-event path); the ring is thread-safe either way. Uninstall restores the vptr
(also at `PreShutdown`).

## Linux vs Wine

Everything above targets the **Windows** build under Proton (the bridge is a Windows tool plugin).
The Linux client has the same schema system (`libschemasystem.so` exports `InstallSchemaBindings`
`[V]` ISchemaSystem.md) and entity system, but no tool-plugin admission and no bridge; a Linux hook
would be an `LD_PRELOAD`/`.so` port with its own locators (`sigcheck` is PE-only today) — out of scope.

## Verification — live run 2026-10-11 (Wine tools, de_dust2, parked `sandbox`, bridge m2.9)

Procedure: `worknotes/vtest/run_vtest.sh sandbox` + `load_bridge.sh`, then `research/game_state_probe.py`
(replies under `/tmp/cs2mcp-live/probe*/`). Three bridge iterations were needed:

1. Every schema read failed `SchemaUnavailable`: `game.schemaProbe` showed the `client.dll` scope *was*
   found; the bug was reading `IsGlobalScope`'s `bool` (`sete al`) through an `int` typedef (garbage upper
   bits). Fixed (`unsigned char` for `IsGlobalScope` and `IsInGame`). Lesson for every vslot call: bool
   returns must be declared as such.
2. `viewAngles` read zeros: see the withdrawn `CG_VIEWANGLES` row; replaced by the pawn's eye angles.
3. The probe selected slots the player did not hold; fixed the probe (slot 3 → knife, slot 2 → pistol).

Observed `[V runtime]`: `game.local` map `de_dust2`, tick 628, origin `[351.4, 2352.9, −120.5]`, eye
`+63.7` z, active `weapon_hkp2000` (defIndex 32, clip1 13, reserve 4); `+forward` for 1 s moved the origin
194 u; `game.weapons` count 2 (`weapon_knife` defIndex 42, `weapon_hkp2000`); `game.radar` 6 ms, one
controller (`cs2-docker`, team 3, health 100, `isLocal`), eyeAngles tracked the override; `sound.*`
wrap after a scripted jump + shot: `Default.WalkJump`, `Weapon_hkp2000.Single`, `Concrete.BulletImpact`,
`Gear.JumpLand.CT`, `Land_Sand.StepLeft` with `entindex` 303 (the local pawn) and its origin; `sound.uninstall`
restores. The slot-81 **listener recorded 0 events in 3 runs** while the wrap recorded 3–6 for the same
actions: its slot-5 callback is reached only from the operator-executed start path (`sub_180177700`/
`sub_18017F040` have no direct callers = operator virtuals), not from top-level `StartSoundEvent*` calls,
so it is **not** a complete event sink. Kept as `mode:"listener"` for experimentation; wrap stays default.

Final run (`research/evidence/2026-10-11-game-state-probe.json`): **35/38** checks; the three fails are
the two listener checks (above) and `game.selectWeapon {weapon:"weapon_knife"}` (`use weapon_knife` queued
but the active weapon stayed the pistol — `slot3`/`slot2` switched knife ↔ pistol correctly). Cause found
afterwards: CS2 has no `use` command; the `weapon` form now goes through `MakeWeaponSelection` (see
"Weapon selection"). Re-run with that change: **39/41**
(`research/evidence/2026-10-11-game-state-probe-weaponselect.json`) — all five selection checks pass;
the two fails are the listener checks.

### Static status table (pre-run)

| Claim | Status |
|---|---|
| 19 sig entries resolve uniquely on 2000930 | **done** (`sigcheck.py resolve --strict`, `pins.py`, `build.sh` -> `mcp_bridge.dll` m2.9) |
| schema walk layout (+0x24/+0x29/+0x30/+0x38, field +0x10) | `[V runtime]` (all fields resolved; values plausible) |
| entity chunk lookup, identity size 112 | `[V runtime]` (controller 1 → pawn 303, weapons 419/420) |
| local controller global | `[V runtime]`; view-angle global withdrawn |
| all schema field names exist in this build | `[V runtime]` |
| `slotN` via `AddCommandText` selects a weapon | `[V runtime]` slot3 → knife, slot2 → pistol; `use weapon_x` had no effect (no such command in CS2) |
| `weapon`/`defIndex`/`handle` via `MakeWeaponSelection` (slot 9) | `[V runtime]` weapon_knife -> knife, defIndex 32 -> pistol, not-held -> -32602 |
| sound wrap records events; name lookup works | `[V runtime]` (names, hash, entindex 303, entity origin) |
| `game.radar` cost (scan of 16k identities per call) | 3–10 ms round trip with one player |

Next: a parked `sandbox` session (`../../../idabins/worknotes/vtest/run_vtest.sh sandbox` +
`bridge/runtime.sh`-style load of the m2.9 DLL) and `research/game_state_probe.py` (to write)
calling each method; record results here with `[V runtime]`.
