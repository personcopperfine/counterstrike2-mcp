# Sound events + sound entities RE notes (build 2000930)

Facts the sound tools are built on (`list_soundevents`, `edit_soundevents`,
`compile_soundevents`, `create_sound_entities`, `drain_sounds`) and bridge m2.9/`sound.drain`
+ m2.20/packed params. Distilled from the RE + live session's notes; post-implementation facts
(the live acceptance verdicts) are in `hammer-mcp/AGENT.md` "## Sounds" "Live acceptance".

**Never copy a number from this file into code**: the bridge's offsets for the packed-params
pointer/length are sig-DB entries (`sigs/signatures/soundsystem.json`,
`SNOFF_SEP_PACKED_PTR` / `SNOFF_SEP_PACKED_LEN` -> `bridge/bridge_structs.h`); everything else
here is either evidence or an engine-internal the sig DB does not need.

Evidence tags: **[V live]** seen in the running tools game/Hammer, **[V bin]** read in the
decompiled/disassembled binary, **[V file]** read in a data file, **[I]** inferred.

---------------------------------------------------------------------------------------------------

## Q1. The compile route [V live]

`resourcecompiler.exe -nop4 -i /app/content/csgo_addons/<addon>/soundevents/<file>.vsndevts`
(inside a tools container, `cs2mcp.tool` label) writes the sibling
`<file>.vsndevts_c` resource container (RED2 + DATA blocks), ~1 s,
` OK: 1 compiled, 0 failed`. `compile_soundevents` runs the same exec with the `-game
/app/game/csgo` form; a synthesized `_c` decodes through `crate::resource` + `decode_kv3` like
every other compiled block.

- Syntax error: `Z:\app\content\...\zz_bad1.vsndevts(5,12): RESOURCE COMPILE ERROR: Invalid
  value "="` (+ `Invalid data for member 'volume'`, `... '"mcp.bad.syntax"'`), `[FAIL]`,
  ` ERROR: 0 compiled, 1 failed, 0 skipped` — `errors[{line, col, message}]` parses these lines,
  the `Z:\app` Windows prefix is stripped.
- **The compiler does NOT check references**: unknown `base`, unknown key (`not_a_real_key`),
  missing vsnd (`sounds/no/such_file.vsnd`), unknown child in `soundevent_01` all compile
  ` OK: 1 compiled`. Our `edit_soundevents` checks (base resolvable, known property keys,
  vsnd/child warnings) are therefore the ONLY ones; every unknown is silent at runtime.
- A compiled `_c` keeps `base` unresolved (the bases' bodies are compiled into the same file
  when they are in it; the runtime resolves the chain).
- A file named anything `.vsndevts` compiles too; only `soundevents_addon.vsndevts` is loaded
  by the game (the template comment; not tested for other names).

## Q2. Runtime loading [V live]

`compile_map` + `run_map` on an addon map loads the addon's
`game/csgo_addons/<addon>/soundevents/soundevents_addon.vsndevts_c` automatically — no extra
step (no manifest edit, no VPK repack). Names resolve through `ISoundEventManager`, so
`drain_sounds` reports the NAME, not just a hash.

## Q3. Runtime observation via the sound-op ring [V live + V bin]

- `sound.status` (or the first `sound.drain`) installs the client-side wrap around
  `ISoundOpSystem`; startOnSpawn events of a load that happens afterwards are then captured.
- A `snd_event_*` started by `startOnSpawn` arrives as
  `{name, via:11, entindex:0 (worldent)}`: the sound entities are **server-only** (the client
  wrap cannot name them, `game.entities` does not see them) and the *entity* origin is not the
  event's location. They fire again on every round restart (entities respawn).
- A logic_timer's `OnTimer -> StartSound` shows up through the same slot; timing is observable
  via `ageMs`.
- A soundscape (`env_soundscape`, `enablesoundevent 1, soundevent <parent>`): the **PARENT**
  event is started (via 11) at round start and again on player re-enter; the CHILDREN
  (`soundevent_01`) start inside the operator stack, never via slots 11/12/13, and are never
  visible to the wrap. `cl_soundscape_printdebuginfo` lists only the legacy `.vsc` registry
  ("CLIENT SOUNDSCAPES (0 total)") — useless for event soundscapes;
  `sv_soundscape_printdebuginfo` lists the server soundscape entities.

### Position from the packed params (bridge m2.20) [V live + V bin]

Slot 11's argument is the server's `SosStartParams_t`: **+0x08** = pointer to the server's
packed-params buffer, **+0x1C** = its length (client!0x1803D0370 builds it from
`CMsgSosStartSoundEvent` msg+72 `std::string packed_params` -> a8/a9;
soundsystem!`CSosOperatorSystem::StartSoundEventWithParams` 0x1801C5C67 copies them into the
call). These two displacements are sig-DB entries — `SNOFF_SEP_PACKED_PTR` / `SNOFF_SEP_PACKED_LEN`
(sigs/signatures/soundsystem.json, resolved through pins.py into bridge_structs.h); the sig
candidate (masked) was `44 8B 43 ?? 45 85 C0 7E ?? 48 8B 53 ?? 48 8D 4F ?? E8` (len disp @3,
ptr disp @12). Bridge m2.20 copies at most 128 bytes into the ring entry as
`params {len, hex, truncated}` (null when the pointer is unreadable, absent on slots 12/13/81).

**Entry framing** [V live]: `u32 hash LE | u8 type | u16 size LE | data[size]` — decoders must
check the walk sums to `len` exactly and otherwise report undecoded. Multi-entry buffers were
seen live (2026-10-16 final run, round start): `Player.PickupWeaponAudible`,
`BaseCombatCharacter.AmmoPickup`, `Player.PickupC4` carry `len 30` = the 19-byte `position` entry
+ an 11-byte entry `hash 0xD91BE141, type 2, size 4` (4 opaque bytes, meaning unknown), and the
walk ends exactly at 30. The `snd_event_*` starts of our map carry only `position` (`len 19`);
a soundscape's parent start carries none (`len 0`).

**The position hash is `0x5A7CCE4D`** [V bin]: soundsystem!`CC_snd_sos_start_soundevent_at_pos`
calls `SetSoundEventParam(guid, 0x5A7CCE4D, {count 1, type 10, size 12, float[3]})`. A live
probe captured, plen 19: `4dce7c5a 0a 0c00 00006c44 00002043 00008042` = hash 0x5A7CCE4D, type
0x0a (vector), size 12, floats (944, 160, 64) — exactly the `snd_event_point`'s Hammer origin;
the other probe entities matched their origins likewise.

### Server soundscape entity fields [V bin, server.dll]

`CEnvSoundscape`: `radius` +0x4C0, `m_soundEventName` +0x4C8, `m_bOverrideWithEvent` +0x4D0,
`m_positionNames` +0x4E0, `m_bDisabled` +0x524, `m_soundscapeName` +0x528, event hash +0x530.
Activation (`sub_1805C21D0`): not disabled && gamerules vfunc 43 && `dist < radius` (or radius
-1) && LOS -> writes the player's audio params (soundscape index, entity, event hash), which
the client plays.

**DSP warning**: the precache path (`sub_1805A8070`) logs
`"Soundscape warning: Event '%s' is being triggered by a soundscape entity and does not have
dsp_override enabled!"` (log channel, not seen in the console in our run) when the event's
`override_dsp_preset` is false — the `amb.soundscapeParent.base` template sets it.

## Q4. Hammer behaviour (the editor) [V live]

`entity.create` of `snd_event_point`, `snd_event_alignedbox` (`box_mins`/`box_maxs`
`"-32 -32 -16"`/`"32 32 16"`), `snd_soundscape` (`radius 600`, `enablesoundevent 1`,
`soundevent`) and `logic_timer` applied every key verbatim, no warnings; the FGD defaults were
filled around them (`stopOnNew "1"`, `soundscape "Nothing"`, `StartDisabled "0"`,
`entityIndexSelection "0"`). Hammer does **not** validate soundevent names (any text is
accepted — the tools' index check is the only one). `entity.addConnections`
`OnTimer -> StartSound` landed; the map saved, compiled and ran.

## Q5. Where the facts live

No bridge method exists for listing a Hammer addon's soundevents — the tool side indexes the
files instead (`list_soundevents` / the soundevent index). The only bridge item in the sound
block is the `sound.drain` ring (m2.9) plus the m2.20 packed-params exposure above; the
`SosStartParams_t` offsets were measured in `bridge_structs.h` terms, never hand-copied.

## Open risks / still unverified

- Multi-entry packed-params buffers have not been observed live; the framing is [I] beyond the
  one-entry `position` case, and the decoder tools refuse-and-report anything that does not
  sum to `len` exactly.
- `override_dsp_preset` is not checked or warned about by our tools yet (the engine logs, we
  do not).
- `cl_soundscape_printdebuginfo`/`sv_soundscape_printdebuginfo` cover only the legacy `.vsc`
  registry and the server entity list respectively — event soundscape children are observable
  only indirectly (they never cross the drained ring).
