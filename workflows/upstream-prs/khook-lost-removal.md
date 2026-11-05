# Draft issue for alliedmodders/khook — a removal racing the insert thread loses the hook without calling its removal function

**Status: draft only, not filed** (AGENTS.md rule 4: the user files it, or asks us to).
Target: `alliedmodders/khook` (as vendored by metamod-source `170058a`), commit `0ca2b01363aff9fa11ad73b1b08dfdc2a195fb3a`.
Line numbers are `src/detour.cpp` at that commit.

## Summary

`RemoveHook(id, …)` can run while the insert thread holds `(id, details)` popped off `insert_hooks` but has not yet
inserted it into the capsule. In that window the removal finds `id` associated, the capsule has no callback for it,
the capsule answers "removed" **without calling the hook's removal function**, and the association is erased. The
insert thread then drops `details`. The hook is never inserted (no use-after-free), but the removal function the
plugin registered is never called, so whatever the plugin frees or counts there leaks. A variant re-queues a stale
`(id, details)` that a later `RemoveHook(id)` scan then *does* call the removal function for, after the plugin was
told nothing would.

## Where

* Insert thread, `:1603-1628`: pops under `insert_hooks_mutex` (`:1607-1610`), **unlocks** (`:1613`), calls
  `__InsertHook_Sync` (`:1615`), relocks, and on failure re-queues (`:1620-1623`).
* `__InsertHook_Sync`, `:1563-1576`: takes `associated_hooks_mutex` shared; returns true ("done") when `id` is not
  associated (`:1567-1569`); otherwise `DetourCapsule::InsertHook`, whose `_detour_mutex.try_lock()` may fail
  (`:1453`).
* `RemoveHook`, `:1806-1866`: the `insert_hooks` scan (`:1812-1840`) runs the removal function only for an id still
  in the list; then `__RemoveHook_Sync` (`:1578-1601`) → `DetourCapsule::RemoveHook` (`:1497-1543`), which returns
  true when `_callbacks.find(id)` misses (`:1508`, `:1543`) — without calling `hook_fn_remove` — and the caller erases
  the association (`:1588`).

## Interleavings

1. **Lost:** insert thread pops `(id, d)` and unlocks → `RemoveHook(id, false)`: scan misses, associated, capsule has no
   callback → true, association erased, removal fn not called → insert thread: not associated → drops `d`.
2. **Stale re-queue:** insert thread pops `(id, d)`, `__InsertHook_Sync` fails `try_lock` (a hooked call is in flight)
   and returns false → before the insert thread relocks `insert_hooks_mutex`, a removal (waiting on the same capsule)
   erases the association → insert thread pushes `(id, d)` back → for up to one loop (5 ms, `:1626`) any
   `RemoveHook(id)` hits the scan and calls the removal function with `d`'s context. Metamod's per-plugin unloader
   re-issues `RemoveHook` for every id a plugin registered, so this is reachable on `meta unload`.

3. **Lost async removal:** the same window on the delete thread: `RemoveHook(id, true)` finds `id` associated and
   queues it (`:1865-1867`); the delete thread's `__RemoveHook_Sync(id, false)` (`:1644`) takes the same not-found
   exit while the insert thread holds `(id, d)` popped.

(1) and (3) need no hooked call in flight: every asynchronous registration on an address that already has a capsule
goes through `insert_hooks` (`:1745-1748`). A synchronous registration only reaches the insert thread when its direct
insert fails `try_lock` (`:1739-1741`), i.e. with a call in flight; (2) always needs one. So they show up most on hooks
of functions called from several threads (e.g. sound threads), and on any async setup.

## Two smaller findings nearby

* **`insert_or_assign` on a fresh address (`:1701`).** Two first registrations on the same address can both miss it
  under the shared lock (`:1695-1698`) and then both take the exclusive lock; the second `insert_or_assign` replaces
  the first thread's capsule, whose destructor (`:1423-1444`) calls the removal functions of the hooks already in it,
  while `associated_hooks` keeps pointing at the destroyed capsule. Suggest `try_emplace` (or re-`find` under the
  exclusive lock) and use the existing capsule.
* **`hook_removal_fn` called twice.** When the scan finds the id (`:1819-1849`) it calls `hook_removal_fn` (`:1845-1847`)
  and breaks out of the loop only, so the function falls through to the async branch (not associated -> called again,
  `:1859-1861`) or the sync branch (`:1873-1875`). Metamod's `Unloader::HookRemoval` tolerates it (it erases from a
  set), but a caller counting completions gets one too many. Suggest `return` after the scan's call.

## Suggested fixes (any one)

* Keep `insert_hooks_mutex` held across `__InsertHook_Sync` in the insert thread (the scan in `RemoveHook` then
  always sees an id that is not yet inserted), or
* have `DetourCapsule::RemoveHook` report "not found" distinctly, and let `__RemoveHook_Sync` leave the association
  in place (and retry) when the id is still pending for the insert thread, or
* mark an id "removed" in `associated_hooks` instead of erasing it, and let the insert thread call the removal
  function when it finds a removed id instead of dropping `details` silently.

## What the cs2-mcp bridge does meanwhile

`s2sdk::hook` (Rust) serialises every Setup/Remove it issues under one mutex, removes synchronously, and treats "id
still tracked after a synchronous `RemoveHook` returned" as lost: it reconciles its count, leaks the context, ignores
a later removal-function call for that id, and re-issues one synchronous `RemoveHook` per lost id before unload to
consume a stale re-queued entry. Proof text: `crates/s2sdk/s2sdk/src/hook.rs`, "The lost-removal guard".
