# Protocol changes in m3.13: `console.exec` in tools sessions, and `cvar.get`

Bridge build `m3.13` is m3.12 plus one gate change and one method (branch `streamline/wp13-console`,
WP1.3 of `workflows/streamline/PLAN.md`). `m3.14` (`protocol-diff-m3.14.md`) is built on it; m3.13 merged after m3.14 was numbered, so an
`m3.14` built from streamline/agent-ux before that merge lacks `cvar.get` and the `-tools` console
gate (hosts gate on the advertised methods).

## 1. `console.exec`: allowed with `-tools` too

`console.exec {command}` -> `{queued:true}` (IInputService `AddCommandText`) was refused with
`-32011 NotImplemented` unless the process command line had `-testscript`. That gate was policy, not
a technical limit. It now accepts **`-testscript` or `-tools`** (the tools framework session
hammer-mcp drives). Everything else is unchanged:

| | m3.12 | m3.13 |
|---|---|---|
| `-testscript` session | queued | queued |
| `-tools` session | `-32011` "console.exec is only enabled in -testscript sessions" | queued |
| any other session | `-32011` | `-32011` "console.exec is only enabled in -testscript or -tools sessions" |

- The flag matcher is the one `-testscript` always used (`cmdline_has_in`: a whole token ending at a
  NUL, space, quote or the end; the start of the match is not anchored, see
  `workflows/streamline/FIXME.md`).
- The command still lands in a 1024-byte buffer (longer is cut), must be non-empty
  (`-32602 command (string) required`), and is never to be sent during a level load. hammer-mcp's
  `console_exec` sends lines of 1024 bytes or more over the VToolMessage channel instead and falls
  back to that channel when this method answers `-32011`.
- Registry: `console.exec` keeps `needs = NEED_ENGINE`, `implemented`, advertised as before (the
  gate runs inside the body, as in the C).

## 2. `cvar.get`

`cvar.get {names:[string] (1..64)}` -> `{cvars:[{name, found, type, value, default, flags:[names],
flagsRaw}]}`, one entry per name, in order. Main thread (`needs = NEED_TIER0`), registered in
`RUST_METHODS`; the handler is `crates/cs2-bridge/src/ops/cvar.rs`.

- A name the engine does not know is `{name, found:false}`. `FindConVar(name, allow_defensive=false)`
  hides a devonly cvar exactly like the in-game console, so a devonly cvar also reads
  `found:false`. `{name, found:false, error}` is a registered cvar whose `GetConVarData` answered
  null (not seen).
- `type` is the engine's own type name (`bool`, `int16`..`uint64`, `float32`, `float64`, `string`,
  `color`, `vector2`..`vector4`, `qangle`, `vectorws`; `unknown` for anything else). `value` and
  `default` are text in the engine's `ValueToString` form (bool `true`/`false`, decimal integers,
  shortest round-trip floats, colour `r g b` or `r g b a`, vectors space-separated); a string over
  1024 bytes is cut with `valueTruncated:true`. `default` is `null` when the record has none.
- `flags` names the bits `devonly hidden protected archive notify replicated cheat release`;
  `flagsRaw` is the whole 64-bit field (other bits have no name).
- A `protected` cvar (passwords) answers `value:null, default:null, redacted:true`. This is
  best-effort output hygiene, not a secret boundary: `console_exec <name>` can still print a
  protected value, and setting one is not prevented (only `rcon_password` is refused, by the
  host's guard).
- Errors: `-32602 InvalidParams` (names missing, 0 or more than 64, a non-string, empty, longer than
  128 bytes or with a control character; checked before the engine is touched), `-32017
  PrerequisiteFailed` ("VEngineCvar007 is not the pinned CCvar") when the object or a called slot
  does not match the pins.

How the pointer rule is met (`workflows/streamline/re/cvar-re.md` is the RE record):

| Needed | Source | Binary verification on Win64 |
|---|---|---|
| the object | `s2sdk::module::versions::CVAR_INTERFACE_VERSION` ("VEngineCvar007", upstream `interfaces.h`) from tier0's `CreateInterface` | `obj_is` against the new pin `T0V_CCVAR_VT` (sigs `T0V_CCVAR_VT`, formerly the entry `CCvar`) and its two called slots `T0VS_CVAR_FIND` / `T0VS_CVAR_GET_DATA` (runtime proof rows in `pins.rs`) |
| `ICvar::FindConVar` slot 11, hidden return pointer for the 8-byte `ConVarRef`, `allow_defensive=false` | generated `ICvar_slots::FindConVar` / `ICvar_vtable::FindConVar` (MSVC shape) | `sdk: S2_SLOT_ICvar_FindConVar` PASS; call shape read at `0x18006C6B0` (RE section 1) |
| `ICvar::GetConVarData` slot 41, `ConVarRef` by value | generated | `sdk: S2_SLOT_ICvar_GetConVarData` PASS; RE section 2 |
| `ConVarData::m_eVarType`, `m_nFlags`, `m_defaultValue`, `m_Values` | generated struct fields (raw `addr_of!` reads, the type as `i16`) | sigs `offset` entries with `sdk` ties, PASS (RE section 3) |
| flag names | generated `FCVAR_*` consts | sigs `const` entries with `sdk` ties, PASS (RE section 4) |

Registry/sigs side effects: the vtable entry `CCvar` in `sigs/signatures/tier0.json` is now keyed
`T0V_CCVAR_VT` (a bridge entry's key is its macro name, as `T0V_TESTSCRIPTMGR_VT`); its 41
`"vtable"` references, one `"base"`, and the four `CCvar` rows in `sigs/scripts/sdkcheck.py`
(`vt:*@tier0.dll/T0V_CCVAR_VT`) follow. `bridge_pins.h` and `generated/pins.rs` gained the pin, two
`PinBytes`/`PinSlot` rows and one `PinIdent`.

Not read live yet: the three engine calls against a running tools session (see
`workflows/streamline/FIXME.md`, WP1.3).
