# Upstream PR draft: `ENT_ENTRY_MASK` contradicts `CEntityHandle`'s bitfields

Target: alliedmodders/s2sdk, branch `cs2`. Status: **draft, not opened.** Opening it needs the
user's go-ahead (AGENTS.md rule 4).

Pinned upstream commit: `b656b5c04cac57313e87d59c4b5a4736db70b87c` (`crates/s2sdk/s2sdk-sys/UPSTREAM.lock`).

## Problem

`public/const.h:76-82`:

```c
#define NUM_ENT_ENTRY_BITS		(MAX_EDICT_BITS + 2)
#define NUM_ENT_ENTRIES			(1 << NUM_ENT_ENTRY_BITS)
...
#define NUM_SERIAL_NUM_BITS		17 // (32 - NUM_ENT_ENTRY_BITS)
#define NUM_SERIAL_NUM_SHIFT_BITS (32 - NUM_SERIAL_NUM_BITS)
#define ENT_ENTRY_MASK			(( 1 << NUM_SERIAL_NUM_BITS) - 1)
```

`public/entityhandle.h:63-64`:

```c
uint32 m_EntityIndex : 15;
uint32 m_Serial : 17;
```

- `ENT_ENTRY_MASK` is meant to mask the **entry (index)** part of a handle, but it is built from
  the **serial** width: `(1 << 17) - 1 = 0x1FFFF`. The index field is 15 bits wide, so the mask
  should be `0x7FFF`. Applying `ENT_ENTRY_MASK` to a handle leaks the low 2 serial bits into the
  index.
- `NUM_ENT_ENTRY_BITS` is `MAX_EDICT_BITS + 2` = 16, which also disagrees with the 15-bit
  bitfield. The trailing comment `(32 - NUM_ENT_ENTRY_BITS)` no longer matches the hard-coded 17.

## Evidence

- Within upstream, the two headers disagree with each other. That is a fact about the source,
  not a guess about the game.
- `cs2-mcp` reads entity identities through a mask derived from the bitfield width
  (`ENTITY_INDEX_MASK = (1 << 15) - 1`, generated from clang's layout of `CEntityHandle`). The
  bridge's `game.entities` / `game.local` / `entity_find` resolve live through it, and the
  results are correct (live runs 2026-10-12, map `editor/zoo/smartprop_zoo`: `cs_gamerules` 1,
  `point_worldtext` 5, with the expected entindex values).
- Upstream does not use `ENT_ENTRY_MASK` itself (grep of the tree at the pinned commit). Only
  plugin code that copies it is affected.

## Proposed change

```diff
-#define NUM_ENT_ENTRY_BITS		(MAX_EDICT_BITS + 2)
+#define NUM_ENT_ENTRY_BITS		(MAX_EDICT_BITS + 1)	// 15, == CEntityHandle::m_EntityIndex
 #define NUM_ENT_ENTRIES			(1 << NUM_ENT_ENTRY_BITS)
 #define INVALID_EHANDLE_INDEX	0xFFFFFFFF

-#define NUM_SERIAL_NUM_BITS		17 // (32 - NUM_ENT_ENTRY_BITS)
+#define NUM_SERIAL_NUM_BITS		(32 - NUM_ENT_ENTRY_BITS)	// 17, == CEntityHandle::m_Serial
 #define NUM_SERIAL_NUM_SHIFT_BITS (32 - NUM_SERIAL_NUM_BITS)
-#define ENT_ENTRY_MASK			(( 1 << NUM_SERIAL_NUM_BITS) - 1)
+#define ENT_ENTRY_MASK			(( 1 << NUM_ENT_ENTRY_BITS) - 1)
```

At the pinned commit `MAX_EDICT_BITS` is 14 (`const.h:64`), so `+ 1` gives 15; re-check on the
target branch before opening. If the maintainers prefer, tie the widths to the bitfields instead,
with a `static_assert` in `entityhandle.h` that `NUM_ENT_ENTRY_BITS` matches the bitfield width.

**Compatibility:** `NUM_ENT_ENTRIES` changes from 65536 to 32768. Check downstream users
(Metamod:Source, CounterStrikeSharp) for arrays sized by it before proposing this. If that is too
disruptive, the minimal PR changes only `ENT_ENTRY_MASK` and the stale comment.

## Suggested PR text

> **Fix ENT_ENTRY_MASK to mask the entity index, not the serial width**
>
> `ENT_ENTRY_MASK` is `(1 << NUM_SERIAL_NUM_BITS) - 1` = `0x1FFFF`, but `CEntityHandle` stores
> the entry index in a 15-bit bitfield (`m_EntityIndex : 15`, `m_Serial : 17`). Masking a handle
> with it keeps two serial bits in the index. `NUM_ENT_ENTRY_BITS` (16) also disagrees with the
> bitfield. This makes the macros agree with `entityhandle.h`.
