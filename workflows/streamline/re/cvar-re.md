# cvar.get RE (streamline WP1.3, ti_convoy FIXME 4)

IDB: target/ida/win64-2000930/tier0.dll.i64 (build 2000930). Every number below that has an `sdk`
row was re-measured against 2000933 (target/game-bins, tier0.dll sha256:990ea56bf1626d29) by
`sigcheck.py resolve` + `sdkcheck.py` (all PASS). CCvar function RVAs are identical in 2000930 and
2000933 (sigs expects). Tags: [V] read in the binary, [I] inferred.

No header change was needed: convar.h/icvar.h already match the binary, and gen.py already emits
the MSVC hidden-return shape for FindConVar. No fork commit, UPSTREAM.lock untouched.
The only source change: 8 `FCVAR_*` entries in allowlist.toml `[consts]` (+ `tier0/convar.h` in the
consts header list and probe.cpp), so gen.py emits `s2sdk_sys::consts::FCVAR_*` (u64) and
`S2_CONST_FCVAR_*` in bridge/s2sdk_gen.h, then 13 tier0.json `sdk` rows.

## 1. FindConVar Win64 call shape [V]
CCvar::FindConVar = tier0!0x18006C6B0 (slot 11, sdk row PASS 11=11).
```
0x18006c6be mov r12d, r9d        ; 4th arg = bool allow_defensive
0x18006c6c1 mov rax, r8          ; 3rd arg = const char* name (strlen + hash 0x31415926)
0x18006c6c4 mov r14, rdx         ; 2nd arg = hidden ConVarRef* return slot
0x18006c6c7 mov rsi, rcx         ; this
...
0x18006c854 mov [r14], rbx       ; found: qword = zero-extended u16 access index (registered index 0)
0x18006c868 mov [r14], rax       ; rax = 0xFFFFFFFF -> {access 0xFFFF, registered 0} = invalid
0x18006c8a9 mov rax, r14         ; returns the hidden pointer
```
So MSVC returns ConVarRef (8 bytes, user ctors) through a hidden pointer in RDX, as the generated
`#[cfg(windows)] ICvar_vtable::FindConVar` already declares:
`unsafe extern "C" fn(this: *mut ICvar, ret: *mut ConVarRef, name: *const c_char, allow_defensive: bool) -> *mut ConVarRef`.
FindFirstConVar (slot 12) / FindNextConVar (slot 13) use the same hidden-return shape (the
cvarlist-by-flag command 0x180075950 calls `vt+96(this,&buf)` and `vt+104(this,&buf,prev)` and
dereferences the returned pointer) [V].

Filtering inside FindConVar (decompiled):
```
if (data == NULL || (flags & 0x400 /*REFERENCE*/)
    || ((flags & 2 /*DEVELOPMENTONLY*/) && (allow_defensive || !(flags & 0x100000000 /*DEFENSIVE*/))))
    *ret = invalid;
```
=> call with allow_defensive = false. A devonly cvar is only found when it also carries
FCVAR_DEFENSIVE; otherwise found:false (same as the in-game console). [V]

## 2. GetConVarData argument [V]
CCvar::GetConVarData = tier0!0x18006C5F0 (slot 41, PASS 41=41).
```
0x18006c5ff mov rbx, rdx         ; ConVarRef by value in RDX (8 bytes)
0x18006c644 mov edx, 0FFFFh
0x18006c649 cmp bx, dx           ; access index (low u16) == 0xFFFF -> return NULL
0x18006c661/71 mov rbx, [rcx+rax*8] (rax = idx*2) ; 16-byte entries, first qword = ConVarData*
0x18006c6a2 mov rax, rbx
```
Matches the generated `GetConVarData: fn(this: *mut ICvar, cvar: ConVarRef) -> *mut ConVarData`
(Rust passes the 8-byte repr(C) ConVarRef in a register on win64, as MSVC does). Returns NULL for
the invalid ref. Also corroborated by ConVarRefAbstract::ConVarRefAbstract(ref, type) 0x18020EA10
calling `g_pCVar->vt[328/8 = 41](ref)` and storing the result at +8 (= m_ConVarData). [V]

## 3. ConVarData members, CVValue_t, EConVarType
| member | offset | evidence | sdk row (2000933) |
|---|---|---|---|
| m_pszName | 0 | 0x18006EC30 (cvarlist row builder) `**(ref+8)` is the name; 0x18006A880 warning "Parent and child ConVars ... %s" passes `*(char**)data` [V] | none: offset 0 has no displacement to measure |
| m_defaultValue | 8 (CVValue_t*) | 0x18020F149 `mov rcx,[rcx+8]` -> ValueToString(default) [V] | `ConVarData::m_defaultValue` PASS 8 |
| m_minValue/m_maxValue | 16/24 | 0x18006EC30 " min. %s"/" max. %s" when non-null [V] | none (not needed) |
| m_pszHelpString | 32 | 0x18006EC30 reads +32 as the help text [V] | none |
| m_eVarType | 40 (int16) | 0x18020F144 `movsx rax, word ptr [rcx+28h]` indexes the type-traits table [V] | `ConVarData::m_eVarType` PASS 40 |
| m_nFlags | 48 (u64) | 0x18006C82D `mov rcx,[rcx+30h]; bt rcx,0Ah` [V] | `ConVarData::m_nFlags` PASS 48 |
| m_Values | 88 | 0x18020F354 `lea r9,[r10+58h]` = slot-0 current value [V] | `ConVarData::m_Values` PASS 88 |
| ConVarRefAbstract::m_ConVarData | 8 | 0x18020F140 first `mov rcx,[rcx+8]` [V] | PASS 8 |

GetValueAsString(slot) 0x18020F340: value = data+88; if FCVAR_PER_USER (bit 15) and 0<=slot<maxslots,
value += slot*traits.size; slot>0 without PER_USER (or a NULL result) falls back to m_defaultValue.
For cvar.get read slot 0: `data + S2_OFF_ConVarData_m_Values` interpreted as CVValue_t. [V]

EConVarType [V]: the type-traits table at tier0!0x180398610 (88-byte entries, indexed by
m_eVarType) has entry names bool, int16, uint16, int32, uint32, int64, uint64, float32, float64,
string, color, vector2, vector3, vector4, qangle, vectorws for 0..15 = the header enum exactly
(generated `EConVarType_*` consts). Not sdk-tied: enum members are not emitted into s2sdk_gen.h, so
sdkcheck has no macro to compare; evidence only.

CVValue_t (16 bytes, union at offset 0) and the engine's own ValueToString (traits slot +0x30) [V]:
| type | read | engine format |
|---|---|---|
| Bool | u8 at +0 | "true"/"false" (0x1802106A0) |
| Int16/UInt16/Int32/UInt32 | i16/u16/i32/u32 | "%d"/"%u" (0x180210360 int32 "%d") |
| Int64/UInt64 | i64/u64 | "%lld"/"%llu" [I: format not read] |
| Float32 | f32 | V_FloatToString (0x1802103A0) - shortest round-trip; Rust `{}` on f32 is fine |
| Float64 | f64 | V_DoubleToString (0x1802103E0) |
| String | `*(const char**)` at +0 (CUtlString), NULL -> "" (0x1802105A0) |
| Color | 4 bytes r,g,b,a | "%d %d %d" if a==255 else "%d %d %d %d" (0x180210600) |
| Vector2/Vector3/Vector4/QAngle/VectorWS | 2/3/4/3/3 f32 | space-separated floats "%s %s %s" (0x180210480) |
Default value: same decoding at `*(CVValue_t**)(data+8)` (non-null per header; the bridge should
still null-check).

## 4. FCVAR bits [V]
tier0 has two flag-name tables:
* 0x18039A100: 23 entries {u64 bit; long; short} used by the cvarlist-by-flag command
  (0x180075950, `lea r10, off_18039A108`, loop `< 0x17`): DEVELOPMENTONLY 0x2/devonly, GAMEDLL 0x4,
  CLIENTDLL 0x8, PROTECTED 0x20/prot, SPONLY 0x40, ARCHIVE 0x80/a, NOTIFY 0x100/nf, USERINFO 0x200,
  UNLOGGED 0x800, REPLICATED 0x2000/rep, CHEAT 0x4000/cheat, PER_USER 0x8000, DEMO 0x10000,
  DONTRECORD 0x20000, RELEASE 0x80000/release, MENUBAR_ITEM 0x100000, LINKED_CONCOMMAND 0x1,
  VCONSOLE_FUZZY_MATCHING 0x800000, SERVER_CAN_EXECUTE 0x1000000, SERVER_CANNOT_QUERY 0x4000000,
  VCONSOLE_SET_FOCUS 0x8000000, CLIENTCMD_CAN_EXECUTE 0x10000000, EXECUTE_PER_TICK 0x20000000.
* 0x18039A050: 11 {bit; short} used by the cvarlist row printer (game, client, archive, notify,
  singleplayer, cheat, replicated, server_can_execute, clientcmd_can_execute, userinfo, per_user).
* HIDDEN (0x10) is in neither table; 0x18020F7D0 (cvarlist skip) returns true for
  flags & (0x400 | 0x10 | 0x2), i.e. reference / hidden / devonly are not listed. [V bit, name from header]
* REFERENCE 0x400 and DEFENSIVE 1<<32: FindConVar (above). [V, not tied: shift/bt measurements]

| flag | bit | sdk row | locator |
|---|---|---|---|
| FCVAR_DEVELOPMENTONLY | 0x2 | PASS | table entry 0 |
| FCVAR_HIDDEN | 0x10 | PASS | `test al,10h` at 0x18020F7E4 |
| FCVAR_PROTECTED | 0x20 | PASS | table entry 3 |
| FCVAR_ARCHIVE | 0x80 | PASS | table entry 5 |
| FCVAR_NOTIFY | 0x100 | PASS | table entry 6 |
| FCVAR_REPLICATED | 0x2000 | PASS | table entry 9 |
| FCVAR_CHEAT | 0x4000 | PASS | table entry 10 |
| FCVAR_RELEASE | 0x80000 | PASS | table entry 14 |
The table locators read the bit by index (rip -> +24k-8 -> qword); the resolver cannot check the
name pointer, so a reorder of Valve's table would show as an expect/sdkcheck FAIL, not silently.

## Checks (worktree sl-wp13)
gen.py; gen.py --check; gen_selftest 422 passed; sdkcheck 308 checks: 288 PASS, 20 SKIP, 0 FAIL
(13 new rows PASS); sdkcheck --self-test ok (linux skipped: 2000930 linux bins not staged);
sigcheck validate 0 problems; resolve --strict all ok (tier0 229 ok); pins.py --check --rust up to date.
No cargo run.
