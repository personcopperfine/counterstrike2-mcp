# Proposed protocol changes for bridge m2.10 (round state + entity lookup)

Additive over m2.9; no major bump, no new pins (`PIN_CLIENT`, `PIN_ENGINE2`, `PIN_SCHEMA` only). Every new
field is schema-resolved at runtime (client.dll scope, `docs/interfaces/ISchemaSystem.md`); a field that does
not resolve is `null`, never a guessed value. Design: `target/orch/unit-test-blocks-design.md` sections 2, 5b, 7 (step 5).

## §7 method table — new and changed rows

| Method | Status | Bridge m2.10 | Params | Result | Call (page) | Notes |
|---|---|---|---|---|---|---|
| `game.rules` | ready [I: not yet run live] | implemented | `{}` | `{tick, inGame, map, proxyIndex, warmup, freeze, teamIntro, gameRestart, hasMatchStarted, gamePhase, roundsPlayed, roundStartTime, restartRoundTime, roundTimeSec, roundWinStatus, roundEndReason, bombPlanted, schemaMissing[]}` | identity with designer name `cs_gamerules` (cached index, re-validated by name); `C_CSGameRulesProxy::m_pGameRules`; fields on `C_CSGameRules`: `m_bWarmupPeriod`, `m_bFreezePeriod`, `m_bTeamIntroPeriod`, `m_bGameRestart`, `m_bHasMatchStarted`, `m_gamePhase`, `m_totalRoundsPlayed`, `m_fRoundStartTime`, `m_flRestartRoundTime`, `m_iRoundTime`, `m_iRoundWinStatus`, `m_eRoundEndReason`, `m_bBombPlanted` | read-only, main thread. `-32025 NoGameRules` when the proxy or its rules object is absent. `schemaMissing` lists the fields that came back null. |
| `game.entities` | ready [I: not yet run live] | implemented | `{class?: designer name, name?: targetname, max?: 1..256 (default 64; counts matches), entindex?: 1..16383 (additive: reads only that slot, count 0 or 1, no scan)}` | `{tick, inGame, map, entities:[{entindex, class, name, origin, angles, health, toggleState}], count, truncated}` | identities 1..0x3FFF (`CEntityIdentity::m_designerName`, `m_name`); origin/angles via `C_BaseEntity::m_pGameSceneNode` (`m_vecAbsOrigin`, `m_angAbsRotation`); `C_BaseEntity::m_iHealth`; `C_BaseToggle::m_toggle_state` | read-only. `name` null when empty. `truncated` true when more matches exist past `max` (never with `entindex`). An out-of-range `entindex` is InvalidParams. `toggleState` is null: the client schema dump has no `C_BaseToggle` fields (`m_toggle_state` exists only in server.dll, offset 0x850, not readable through the client scope). |
| `game.local` | changed (additive) | implemented | `{}` | `player` gains `armor`, `helmet`, `money`, `ducked`, `moveType` | `C_CSPlayerPawn::m_ArmorValue` (int32); `CCSPlayerController::m_bPawnHasHelmet` (bool); `CCSPlayerController::m_pInGameMoneyServices` -> `CCSPlayerController_InGameMoneyServices::m_iAccount` (int32); `C_BasePlayerPawn::m_pMovementServices` -> `CCSPlayer_MovementServices::m_bDucked` (bool); `C_BaseEntity::m_MoveType` (uint8) | each null when unresolved or no pawn. `moveType` is the raw MoveType_t value. [I] |

## §8 error codes — new

| Code | Name | Meaning |
|---|---|---|
| -32025 | `NoGameRules` | no `cs_gamerules` entity with a `C_CSGameRules` object (not in a game); nothing was executed |

`-32023 SchemaUnavailable` (m2.9) is also returned by `game.rules` when `C_CSGameRulesProxy::m_pGameRules` is absent from the schema.

## §5/§6 notes
- Method count: 59 registered / 41 implemented (m2.9: 57/39), both new methods implemented.
- `game.rules` and `game.entities` run on the tools main thread like every `game.*` read; the reply buffer for
  `game.*` grew to 96 KB (entities: up to 256 x ~250 B).
- `game.entities` overflow: entities are written whole or rolled back; when the reply is near its buffer
  (~64 B kept for the trailer) the rest is dropped and `truncated` is true. The call still succeeds.
- Name strings from game memory (designer name, targetname) are copied into a bounded 256-byte slot
  (readability checked per page) before use; they are never passed unbounded to the JSON writer.
- The cached `cs_gamerules` index is used only while the entity's identity back-pointer agrees with it.
  Schema field misses are negative-cached once the client scope is up (no re-walk per entity).
- `sb_addf` writes `null` for NaN or infinite floats.
- Live-probe list for the first run (`[I]` above): the designer name `cs_gamerules`; the identity `m_name` read
  (`CUtlSymbolLarge` as a `char*`); `m_gamePhase` and `m_iRoundWinStatus` widths (read as int32).
