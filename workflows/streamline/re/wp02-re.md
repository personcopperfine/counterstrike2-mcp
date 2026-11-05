# WP02 RE: ti_convoy FIXME 25 (doc.active dirty stays true after save_map skippedClean)

IDB: target/ida/win64-2000933/hammer.dll.i64 (build 2000933, base 0x180000000). Read-only; nothing renamed or patched.

## Verdict
The hypothesis holds [V]. `HFN_CHANGED_SINCE_SAVE` (0x18097F9C0) returns a `bool` in AL. On both "false"
paths the upper 56 bits of RAX are left over, never zero-extended. The bridge reads the return as a
full i32 (`FnChangedSinceSave -> i32`, `!= 0`, hammer.rs:3448/4651), so `doc.active.dirty` can be
true while Hammer's own answer is false. `HFN_DOC_OR_DEPENDENCY_CHANGED(doc, 0)` zero-extends its
false result (`movzx eax, bl`), so map.save sees "clean" and skips correctly.

## Q1: ChangedSinceSave 0x18097F9C0: `bool (CDocDataManager *mgr, CMapDoc *doc)` [V]
Layout read from the code: mgr = { int count @+0; Entry *entries @+8 }, Entry is 0x38 bytes:
{ int n @+8; Elem *elems @+0x10; u64 counter @+0x20; u64 savedMark @+0x28 }. Elem is 0x20 bytes: { void *doc @+0; u64 mark @+0x10 }.

```
0x18097f9ca  movsxd rax, dword ptr [rcx]      ; rax = mgr->count
0x18097f9d6  test eax,eax / jle 0x18097FA2A   ; empty manager -> false (rax = count <= 0)
... outer loop over entries r8, inner loop over elems:
0x18097f9e6  mov rax, [rdi+8]                 ; rax = entries (POINTER)
0x18097f9f0  add rax, rcx
0x18097f9fe  mov rax, [rax+10h]               ; rax = elems (POINTER)
0x18097fa02  cmp [rax], r10 ; jz found
0x18097fa0c  add rax, 20h                     ; rax walks the elem array
...
0x18097fa2a  xor al, al                       ; FALSE path 1: only AL cleared
0x18097fa36  retn                             ;   RAX[63:8] = leftover heap pointer bits
found:
0x18097fa60  mov rax, [rax+28h]               ; rax = entry->savedMark (u64)
0x18097fa64  cmp [rdx+rcx+10h], rax           ; elem->mark vs savedMark
0x18097fa69  setnbe al                        ; AL = elem.mark > savedMark (unsigned)
0x18097fa6c  retn                             ; FALSE/TRUE path 2: RAX[63:8] = savedMark[63:8]
```
- False path 1: the doc is not in any entry, or a negative count. EAX's upper 24 bits come from a heap
  pointer (entries or elems), which is nonzero in practice. The one exception is count == 0:
  then rax = 0 and EAX is clean. [V]
- Path 2: the doc is found. EAX = (savedMark & 0xFFFFFF00) | setnbe. When clean, EAX == 0 only while
  savedMark < 256. The counter is a per-entry u64 that rises by 1 on every modification (see Q4), so
  after 256 or more edits in the entry's lifetime a clean document reads as dirty to the bridge,
  permanently, because the counter never decreases. That matches "forever". [V] for the code,
  [I] that the live session had passed 256 edits (bridge-driven sessions make many edits).

## Q2: DocOrDependencyChanged 0x180886870: `bool (CMapDoc *doc, int selfOnly)` [V]
```
0x18088687a  mov edi, edx                     ; second arg
0x180886882  mov rcx, cs:off_1821DFE60        ; mgr = *(base+0x21DFE60)
0x180886889  call sub_18097F9C0               ; ChangedSinceSave(mgr, doc)
0x18088688e  test al, al                      ; Hammer tests AL only
0x180886890  jnz 0x180886961                  ; -> ret with ChangedSinceSave's RAX (AL != 0, so nonzero; OK)
0x180886896  cmp edi, 1
0x180886899  jz  0x180886961                  ; arg==1: return ChangedSinceSave's RAX AS-IS (garbage upper bits even when false!)
0x1808868c3  call sub_18097EC60               ; collect dependency docs into a CUtlVector
0x1808868f0  call sub_18097F9C0 / test al,al / jnz -> mov bl,1   ; per dependency
0x180886901  xor bl, bl
0x18088695e  movzx eax, bl                    ; dependency path: fully defined 0/1
```
- Second parameter: 1 = "this document only" (skip dependencies). Any other value (the bridge passes
  0) = also check dependencies. So 0 means what the bridge wants. [V]
- With arg 0, EAX is well defined when false (`movzx eax, bl`). When true it can carry garbage in the
  upper bits but is nonzero, so the bridge's `!= 0` happens to be correct. With arg 1 the false path
  would leak ChangedSinceSave's garbage, so never call it with 1 while reading an i32. [V]

## Q3: Implication and the manager global [V]
- `off_1821DFE60` is RVA 0x21DFE60 == `HG_DOCDATA_MANAGER` (pins.rs:367). Both read it as a pointer
  (`mov rcx, cs:[...]`), the same as the bridge's `*(base + HG_DOCDATA_MANAGER)` deref.
- DocOrDependencyChanged(doc,0) == 0 is reached only after `test al,al` on ChangedSinceSave(mgr, doc)
  falls through (0x18088688e), so it implies ChangedSinceSave's AL == 0 for the same doc and manager.
  The two bridge readings can only disagree through the upper EAX bits. [V]

## Q4: The marks, for completeness (the hypothesis is not wrong, but here is what Save resets) [V]
- HFN_DOC_SET_MODIFIED = 0x1808911C0 (pins.rs:289). It calls `sub_18097F840(off_1821DFE60, a2, 0)`
  (MarkModified). That function finds the doc's elem, then does
  `entry+0x20 = entry+0x20 + 1; elem+0x10 = entry+0x20` (the decompile of 0x18097F840). This is
  where the counter increases monotonically.
- Save: in 0x180980A20 (the manager's save/path routine), after `sub_180981AF0` returns 0 and
  `V_stricmp_fast` of the paths matches:
  `0x180980c22 mov rax,[rbx+20h]` / `0x180980c26 mov [rbx+28h], rax` (savedMark = counter).
  After a save, elem.mark <= savedMark, so `setnbe` gives 0. Save does reset the logical state, and
  nothing the edit path bumps survives a real save. The only defect is the bridge's read width.
  (Also seen: 0x18097FC20 `inc [rcx+20h]; mov [r15+28h], rax` is a different add/register path;
  not decompiled further, not needed.)

## Q5: Other i32-returning bridge types [V]
- `FnMeshFaceMaterialIndex` -> HFN_MESH_FACE_MATERIAL_INDEX 0x1808D9410: every exit writes full EAX
  (`mov eax,0FFFFFFFFh` / `cmovl eax, edx` at 0x1808d9495-0x1808d949a, `mov eax,0FFFFFFFFh` at
  0x1808d94a3). It is a real int. OK.
- `FnSelCount` -> HFN_SEL_COUNT 0x18089F400: a tail jump to `[vtbl+0xE0]` of the mode's selection set.
  Every CSelectionSet<...> / CObjectSelectionSet / CPoly{Vertex,Edge,Face}SelectionSet vtable has
  +0xE0 = 0x1800BA470: `mov eax, [rcx+8]; retn`, a full int. OK. (The mode > 7 path does
  `xor ecx,ecx; mov rax,[rcx]`, a null deref; the bridge must keep mode within 0..7, which it does
  per the pin note.)

## Recommended fix
Minimal change in crates/cs2-bridge/src/ops/hammer.rs:
- `type FnChangedSinceSave = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8;` (or `-> bool`;
  u8 matches the bridge's convention "every bool is a byte"), keeping the `!= 0` test. The Win64 ABI
  then reads only AL.
- For consistency and safety, make `FnDocOrDependencyChanged` return `u8` too (the game type is
  bool). map.rs `doc_or_dependency_changed` would return `u8`/`bool`, or `i32::from(byte)` to keep the
  trait signature. This is correct today only because the bridge passes 0. If someone passes 1, the
  same bug reappears.
- Update the pin doc text in sigs/signatures/hammer.json for HFN_CHANGED_SINCE_SAVE /
  HFN_DOC_OR_DEPENDENCY_CHANGED to say "returns bool in AL; upper RAX undefined" (then regenerate
  pins; do not edit pins.rs by hand).

Negative tests (unit, no game): a fake ChangedSinceSave `extern "C"` returning e.g.
`0x0000_0100` (AL = 0, upper bits set, like savedMark = 256) and `0xDEAD_BE00` (pointer-like) must
give `dirty:false`; `0x0000_0101` must give `dirty:true`. Do the same for doc_or_dependency_changed.
Live regression: in one session, make 256 or more edits (each SetModified bumps the counter), then
save_map and doc.active. dirty must be false and compile_map must not refuse UnsavedChanges. A doc
not registered in the manager (false path 1) must also read clean.
