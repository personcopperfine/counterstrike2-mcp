# 11 — Game state from inside the game (radar, view, ammo, weapons, sounds)

## Goal
The model reads what the game knows — player/teammate/enemy/bomb/hostage positions as the radar
sees them, the local view and position, ammo, the weapon inventory and selection, and the stream of
sound events — through the bridge, not through pixels. Update-tolerant by construction: entity
fields resolve through the game's SchemaSystem at runtime; only entry points are signature-pinned.

## Prerequisites
- WineTools session with the m2.9+ bridge (`crates/cs2-bridge`, the game-state ops;
  built by `bridge/build-rs.sh`; pins now include `schemasystem.dll` and `soundsystem.dll`).
- Research note: `research/game-state.md` (sources, evidence tags, what is unverified).
- Reading: `../idabins/docs/interfaces/ISchemaSystem.md` (runtime offset recipe), `IVEngineClient2.md`
  (slots 34/39/65), `ISoundOpSystem.md` (slots 11–13, `SosStartParams_t`), `docs/data/bridge-protocol.md`.

## Method spec (proposed §7 rows; full diff in `bridge/protocol-diff-m2.9.md`)

| Method | Needs | Params | Result | Errors |
|---|---|---|---|---|
| `game.local` | client+engine2+schemasystem | `{}` | `{tick, inGame, map, playerSlot, viewAngles:[p,y,r] (pawn eye angles), inputOverride:[p,y,r]\|null, player:{controllerIndex, pawnIndex, name, team, alive, health, isLocal, origin, eyeAngles, spotted, spottedByMask, eye, velocity, lifeState, flags, scoped, activeWeapon:{entindex, handle, name, defIndex, clip1, clip2, reserve, reloading}}}` | `-32023 SchemaUnavailable`, `-32024 NoLocalPlayer`, `-32017` |
| `game.weapons` | same | `{}` | `{tick, inGame, map, active:{weapon}, count, weapons:[weapon]}` | same |
| `game.radar` | same | `{}` | `{tick, inGame, map, players:[{controllerIndex, pawnIndex, name, team, alive, health, isLocal, origin, eyeAngles, spotted, spottedByMask}], bomb:[{state: planted\|carried\|dropped, entindex, origin, ticking?, defused?, site?, blowTime?, ownerPawnIndex?}], hostages:[{entindex, origin, state, rescued, health, leaderPawnIndex}]}` | `SchemaUnavailable`, `-32017` (no local player is not an error here) |
| `game.selectWeapon` | engine2 (+client, schema for the weapon forms) | `{slot:0..13}` \| `{command: invnext\|invprev\|invnextselect\|invprevselect\|lastinv}` \| `{weapon:"weapon_x"}` \| `{defIndex:n}` \| `{handle:h}` | `{queued:true, via:"command", command}` or `{queued:true, via:"MakeWeaponSelection", weapon:{...}}` | `-32602` outside the allow-list or weapon not held; `-32024`; `-32017` |
| `game.schemaProbe` | schemasystem | `{names?:[...]}` | `{results:[{name, found, scopeName, isGlobal}]}` | diagnostics for update drills |
| `sound.status` / `sound.subscribe` | soundsystem | `{mode?: wrap\|listener\|both}` | `{installed, mode, pending, total, dropped, ringSize}` (installs the chosen sink; default wrap) | `-32017`, `-32602` |
| `sound.drain` | soundsystem | `{max?:1..256, resolve?:bool}` | `{events:[{seq, hash, name, entindex, guid, via:11\|12\|13\|81, flags, ageMs, aux?, inst150?, origin?, entityName?}], dropped, total, pending}` | `-32017` |
| `sound.uninstall` | soundsystem | `{}` | status | — |

Threading: every `game.*` read runs on the tools main thread in the drain (same as `input.*`); the
sound wrappers and the slot-81 listener run on the sound update's thread (sos lock held) and only touch a spin-locked ring; `sound.drain` reads the
ring and resolves origins on the main thread. Nothing is executed from the IPC thread.

Value conventions as the protocol: vectors `[x,y,z]` world units, angles `[pitch,yaw,roll]`.
Handles are the game's `CHandle` integers (index `& 0x7FFF` = `s2sdk_sys::generated::ENTITY_INDEX_MASK`, the
15-bit `CEntityHandle::m_EntityIndex` mask), entindex plain ints; `null` where a
schema field could not be read (never a guessed value).

## Steps
1. **Sigs** — done: `bridge/proposed-sigs/{client,engine2,schemasystem,soundsystem}.json` (19 entries,
   `sigcheck.py validate` 0 problems, `resolve --strict` all unique; the `expect[]` values were
   recorded with `sigupdate.py record`, and since the files carry bridge entries `sigcheck.py
   resolve --record` now refuses them outright). `pins.py` overlays
   them; merge into `../idabins/signatures/*.json` when accepted (then delete the proposed files).
2. **Bridge** — done, compiles: `bridge/mcp_bridge_game.inc` (schema resolver + cache, identity chunks,
   local controller, `game.local/weapons/radar/selectWeapon`, sound wrap + ring + `sound.*`),
   `k_methods` rows, `NEED_SCHEMA`/`NEED_SOUND`, `PIN_SCHEMA`/`PIN_SOUND`, uninstall at `PreShutdown`,
   `BRIDGE_BUILD` m2.9.
3. **Tools** — done: `client-mcp/src/tools/game_state.rs` (`game_state`, `weapons`, `radar` + overview
   projection from `--overviews-dir/<map>.txt`, `select_weapon`, `sounds_drain`, `sounds_status`),
   tool groups `game_state`/`sounds` in `session_status`, mock-backed tests
   (`client-mcp/tests/game_state_tools.rs`), new wire codes in `cs2-mcp-common::bridge::errors`.
   `game_wait` (`client-mcp/src/tools/game_wait.rs`, ReadOnly, `game.local` only) waits for a predicate
   over the local player instead of sleeping: `{until: tick, ticks?}`, `{until: moved, minDistance?}`
   (3D distance from the baseline, tick advanced), `{until: stopped}` (speed < 1 on two samples with
   the tick advancing), `{until: weapon, weapon | defIndex}`; optional `from {tick, origin}` baseline
   (tick/moved), `timeoutMs` (default 5000, clamped to the call budget minus 500 ms), `pollMs` 50..1000
   (default 100; sleeps only the rest of each poll interval). Returns `{matched, until, waitedMs,
   samples, before, after, delta}`; a timeout is `WaitTimeout` (retry-safe, with `last`); not in game or
   dead is `PrerequisiteFailed` at once. `select_weapon {weapon}` and `input_set` with a movement
   `press` return a `next` hint pointing at it.
4. **Live verification** — done 2026-10-11 (research note; 35/38, then 39/41 after the `MakeWeaponSelection` weapon path); re-run with bots for `spotted` and with Ts for the bomb. Original plan: parked `sandbox` session + m2.9 DLL; a `research/game_state_probe.py`
   that calls every method and asserts: `game.local.player.origin` matches `getpos` within 1 u;
   `viewAngles` tracks `input_set {angles}`; `weapons.count` matches the buy; `select_weapon {slot:2}`
   changes `active.name`; `radar.players` lists the bots with `spotted` toggling; `sounds_drain` after a
   jump/shot contains `Player.*`/weapon events with the local pawn's entindex. Record in the research
   note and upgrade `[I]`/not-run rows to `[V runtime]`.
5. **Protocol docs** — done 2026-10-11 (signature merge deferred by decision) — the idabins edits listed in `bridge/protocol-diff-m2.9.md` (§7 rows, §8 codes
   -32023/-32024, module list, m2.9 note).
6. **Sound sink** — done (research `research/game-state.md` "Sink survey"): the game's own
   `ISoundOpSystem` slot-81 listener is implemented as `mode:"listener"`; the vtable wrap stays the
   default. Live step: `sounds_status {mode:"both"}`, fire a shot/jump, `sounds_drain`, and compare
   `via:81` records (`inst150`) with `via:11` records (`entindex`) for the same `guid`; if they match,
   flip the default to `listener` and retire the wrap.
7. **Follow-ups**: gear slot per weapon (vdata), packed-field position for sounds, `IGameEventSystem`
   handler for `CMsgSosStartSoundEvent` (networked sounds with positions), Linux port.

## Definition of done
- Step 4 passes on 2000930 and the evidence is recorded; `observe` (08) uses `game_state` instead of
  console `getpos`; `radar` projects correctly on `de_dust2` (visual check against the radar image).

## Open questions
- Does slot 86's instance int `+0x150` equal the source entity index (makes the listener complete)?
  What are callback args `aux0/aux1` (record `+0xB4/+0xB8` -> instance `+0x1FC/+0x204`)?
- Is `ISoundOpSystem` slot 11 called on the main thread only, and are any starts internal (non-vtable)
  calls the wrap misses? Compare wrap vs listener totals in `mode:"both"`, and with `snd_sos_show_soundevent_start 1`.
- Sound positions: parse `SosStartParams_t` packed field data (`+0x08`, count `+0x1C`) for an explicit
  position (fields are hashed names; `ISoundOpSystem` slots 44/45 give the hashes).
- Weapon gear slot (`CCSWeaponBaseVData::m_GearSlot` via the entity's subclass vdata) — needs the
  subclass -> vdata pointer path (`C_BaseEntity::m_nSubclassID` + `GetSubclassData`), unreversed.
- `game.radar` scans 16k identities per call; if it costs > 2 ms, cache the highest index
  (`CGameEntitySystem+0x2120` in cs2-dumper `[D]`, unpinned) or scan only chunks that exist.
- Does `FindTypeScopeForModule("client.dll")` need the scope name with a path or different casing under
  Wine? (`IsGlobalScope` guard reports it as `SchemaUnavailable`.)
- Linux (headless) equivalent: no bridge; would need an `.so` hook and ELF locators.

## Evidence
- Live (2026-10-11, see research note "Verification — live run"): `game.local`/`game.weapons`/`game.radar`/`game.selectWeapon`/`sound.*` (wrap) verified on a parked de_dust2 session; listener mode silent.
- Static: `research/game-state.md` (all `[V]` addresses), `../idabins/docs/interfaces/{ISchemaSystem,IVEngineClient2,ISoundOpSystem}.md`.
- Mechanical: sigcheck/pins/build outputs (0 problems, 19/19 unique, DLL built).
- Runtime: **none yet**.
