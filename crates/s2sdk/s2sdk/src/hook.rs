//! KHook: safe Rust hooks over Metamod:Source 2.0's `IKHook` detour interface.
//!
//! KHook (`third_party/khook/include/khook.hpp`, impl in `src/detour.cpp`) is the detour
//! library Metamod 2.0 ships. A plugin gets its `KHook::IKHook*` from
//! `ISmmAPI::GetDetourInterface(id)`; C++ plugins then use the `KHook::Function<>` /
//! `Member<>` / `Virtual<>` templates, which build the five entry points KHook stores per
//! registration (a `pre`, a `post`, a `make_return` and a `make_call_original` thunk, plus a
//! removal function) and a stack-size number computed from the signature.
//!
//! This module is the Rust side of the same contract. It does **not** call through the
//! `IKHook` vtable itself: it defines [`KHookApi`], one method per `IKHook` virtual with the
//! same names and the same raw-pointer types, and the wrapper and the [`khook_thunks!`]
//! macro work against a `&'static dyn KHookApi` installed in a process global
//! ([`install`]). A later step adds the vtable-backed `impl KHookApi` over the generated
//! `IKHook_vtable`; nothing here needs to change for that.
//!
//! # What the macro generates
//!
//! `khook_thunks!` declares a module holding, for one hooked signature:
//!
//! * the four thunks with the hooked function's exact signature and ABI (`this` first, as
//!   KHook's `Member<>`/`Virtual<>` thunks have) and the removal function,
//! * the stack size computed from the declared parameter list for **both** ABIs
//!   ([`stack_size_win64`], [`stack_size_linux`]), and the per-target [`STACK_SIZE`]-style
//!   constant,
//! * `setup_inline` / `setup_virtual`, which build the per-hook context, register with KHook
//!   and hand back a [`Hook`] that removes the hook in [`Drop`].
//!
//! # The callback contract (what the thunks do)
//!
//! All of it is taken from `khook.hpp` `Function<>` (`:722-801`, the shape `Member<>` and
//! `Virtual<>` repeat) and from the JIT trampoline in `detour.cpp` (`:746-790` pre/post loop,
//! `:886-916` original call, `:936-940` post loop, `:963-973` `make_return`, `:1529-1541`
//! removal) — every claim below was read there:
//!
//! * KHook enters each thunk with the hooked function's *original* machine state (`detour.cpp`
//!   `copy_stack` + `restore_regs` around the call), so the thunk signature must be the hooked
//!   function's signature exactly.
//! * `pre` / `post` walk the hook's callbacks, fold their [`Return`]s by max over [`Action`]
//!   (Ignore 0 < Override 1 < Supersede 2, `khook.hpp:744-752`) and then call
//!   `SaveReturnValue(action, ptr, size, init_op, deinit_op, original = false)`. A `pre` that
//!   answered `Supersede` makes the trampoline skip the original (`detour.cpp:890-893`);
//!   `Override` also overwrites the value but still calls the original.
//! * The `pre`/`post` thunks' *own* return value is not used by the trampoline; the C++
//!   templates hand back their default-constructed `_fake_return` (`khook.hpp:762-765`) and so
//!   does this macro.
//! * `make_call_original` fetches `GetOriginalFunction()`, calls it with the thunk's own
//!   parameters and saves the result with `original = true` (`khook.hpp:791-801`).
//! * `make_return` reads `GetCurrentValuePtr(true)`, calls `DestroyReturnValue()` once and
//!   that value is the hooked call's result (`khook.hpp:779-788`).
//! * The removal function is `extern "C" fn(HookId)`, its context coming from
//!   `GetContextPtr()` — KHook pushes it around the call (`detour.cpp:1536-1541`), exactly as
//!   `khook.hpp:711-719` relies on.
//! * `init_op` / `deinit_op` are `khook.hpp:307-315`'s copy-construct / destroy: for a Rust
//!   return type the first is `ptr::write(dst, *src)` and the second a no-op, because a `Copy`
//!   Rust type has no destructor. The macro's generated code asserts `Return: Copy + Default`
//!   so a return type KHook cannot copy (or for which no dummy exists) fails to compile.
//!
//! # Ownership of the hook context
//!
//! The per-hook context (the callbacks and the dummy return value) is a single `Box`
//! allocation whose first field is a [`CtxHeader`] reference count. Registration adds one
//! reference (KHook's, released by the removal function it will call) and the [`Hook`] holds
//! another (released in [`Drop`], after it asked KHook to remove the hook). Whichever runs last
//! frees it, so the context is never freed while KHook may still call into it, and it is not
//! leaked either. Because KHook's reference is released by whichever thread removes the hook —
//! its delete thread for the asynchronous removal [`Drop`] queues, the caller's own for the
//! synchronous one [`remove_all_blocking`] issues — the context can be freed there, which is why
//! the generated context has to be `Send` (the macro asserts it).
//!
//! # Removal: asynchronous by default, and unload safety
//!
//! Both `SetupHook`/`SetupVirtualHook` and `RemoveHook` take an `async` flag, and this module
//! takes KHook's own answer for both: `async = true`. The C++ templates install and remove
//! asynchronously "for safety reasons" (`khook.hpp:673,685`), because KHook's per-hooked-address
//! mutex is held *shared* for the whole of a hooked call and **removal** needs it exclusively:
//! removing a target that is already on the current call stack spins until that call ends —
//! `__RemoveHook_Sync(id, true)` loops on the capsule's `try_lock` with a 1 ms pause
//! (`detour.cpp:1581-1589,1503,1871`). [`SetupMode`] is the explicit opt-in for callers that
//! know their target is not on any stack.
//!
//! Setup does *not* spin, so the flag is a preference rather than a hazard there: the first
//! registration on a fresh address is made **synchronous whatever the flag says**
//! (`detour.cpp:1712-1717` sets `async = false`), but that does not mean the insert succeeds
//! inside the call: the address is patched before the capsule's mutex is released
//! (`detour.cpp:1705,1710`), so a call can already be in flight on it, `DetourCapsule::InsertHook`'s
//! `try_lock` (`:1453-1455`) fails, and KHook queues the entry for the insert thread instead
//! (`:1738-1748`). A synchronous insert on an existing capsule that cannot take the mutex is
//! queued for the same reason. So "first registration" means "asked to insert synchronously",
//! never "inserted when this returns".
//!
//! Asynchronous removal moves the last use of a hook context onto KHook's delete thread: for
//! every registered hook KHook calls the removal function it was handed, and *that* call is
//! code in this module ([`release_on_removal_by_id`], generated into every `khook_thunks!`
//! module).
//! A plugin that unloads while a removal is still queued therefore has its own code called after
//! unload. [`live_context_count`] and [`remove_all_blocking`] are the answer: a process-global
//! count of contexts KHook can still call into (raised on registration, lowered by the removal
//! function, which is the last thing this module runs for that hook), the set of removals
//! [`Drop`] queued and KHook has not run yet, and a function that removes all of them
//! **synchronously** — rather than waiting for KHook's delete thread, which may be gone or
//! never get to a hook — and then checks that count before the plugin lets its images go.
//!
//! # Re-entrancy and threads
//!
//! KHook runs a hook's thunks on whichever thread reached the hooked function (the trampoline
//! copies the machine state, not a thread), and a callback may itself re-enter the hooked
//! function. Both cases rule out borrowing the callback list while it runs, so each thunk
//! clones the list under its lock and calls the copy ([`dispatch`]) — the same deep copy the
//! C++ templates make for the same reason (`khook.hpp:733-739`), which is why [`PreCb`]
//! callbacks are `Fn + Send + Sync` and keep any state of their own behind their own
//! synchronisation. The context itself is shared safely: the reference count, the callback
//! mutexes and the dummy return value are the only things a thunk touches, and none of them is
//! mutated except under its own synchronisation.
//!
//! A dispatch costs one small `Vec` of `Arc` clones per call, as the C++ deep copy costs one
//! map copy per call.
//!
//! # Panics
//!
//! The thunks are `extern "C"`, so a panicking user callback aborts the process (Rust's
//! abort-on-unwind for non-unwinding ABIs) rather than unwinding through KHook's JIT frames.
//!
//! # Threading
//!
//! `GetContextPtr()` and everything else KHook documents as thread-local is only valid inside
//! a thunk, which is why the thunks fetch the context themselves and never store it. Install
//! hooks on the thread that owns the hooked object and drop the [`Hook`] there; the removal
//! itself may happen on KHook's delete thread, which is what the reference count and [`Send`]
//! bound exist for.
//!
//! # The lost-removal guard
//!
//! KHook (`0ca2b013`) can forget a hook without ever calling its removal function. This section
//! says when, why no lock of ours can prevent it, and why what this module does instead is
//! enough. `K:` lines are `third_party/khook/src/detour.cpp` at that commit; `MM:` lines are
//! Metamod:Source `170058a`.
//!
//! ## KHook's state for one hook id
//!
//! * `associated_hooks[id] -> capsule`, under `associated_hooks_mutex` (a `shared_mutex`);
//! * `insert_hooks`, a list of `(id, details)` waiting for the insert thread, under
//!   `insert_hooks_mutex` (`details` carries our removal function and context);
//! * `delete_hooks`, asynchronous removals waiting for the delete thread (`K:1630-1669`);
//! * the capsule's `_callbacks[id]`, under the capsule's `_detour_mutex`, which every hooked call
//!   in flight holds **shared** (`K:214`, released at `:432`, `:183` or `:197`).
//!
//! Hook ids come from one process-wide counter (`lastest_hook_id++` under `hook_id_mutex`,
//! `K:1729`), and Metamod's per-plugin `IKHook` objects all forward to the same library
//! (`MM: core/metamod_khook.h:49,77`), so an id is never reused, by us or by any other plugin.
//!
//! Our removal function is called in exactly four places:
//!
//! 1. the `insert_hooks` scan at the top of `RemoveHook` (`K:1819-1843`), with
//!    `insert_hooks_mutex` held (the association is erased under `associated_hooks_mutex`, which
//!    is released again at `:1835`, before the call);
//! 2. the capsule's `RemoveHook` (`K:1497-1543`), reached only from `__RemoveHook_Sync`
//!    (`K:1578-1598`) with `associated_hooks_mutex` held exclusively (`:1581-1589`);
//! 3. the capsule destructor (`K:1423-1444`) when `Shutdown` clears the tables (`K:1885-1888`,
//!    process exit);
//! 4. the same destructor when two first registrations on one fresh address race and the second
//!    `insert_or_assign` (`K:1701`) replaces the first capsule.
//!
//! (3) and (4) are outside the argument below: (3) is process exit; (4) is a KHook bug of its own
//! (the association keeps pointing at the destroyed capsule), reported upstream with this one.
//! Our own setups cannot race each other (G1), only another plugin's. The bookkeeping still
//! accounts for both: a removal function that runs for an id after its `SetupHook` returned but
//! before this module recorded it (the window between `setup_hook` and `finish_registration`) is
//! remembered, and the registration then hands back a [`Hook`] that is already gone instead of
//! recording the id.
//!
//! ## The race
//!
//! The insert thread (`K:1603-1628`) pops `(id, details)` under `insert_hooks_mutex`
//! (`:1607-1610`), **unlocks it** (`:1613`), and only then calls `__InsertHook_Sync`
//! (`K:1563-1576`), which takes `associated_hooks_mutex` shared. A synchronous `RemoveHook(id)`
//! that runs between those two steps misses `id` in the scan (it is no longer in the list), finds
//! it associated, and asks the capsule, which has no callback for it yet: the capsule answers
//! "removed" **without calling the removal function** (`K:1508` not found -> `:1543`), and the
//! association is erased (`:1588`). `__InsertHook_Sync` then finds `id` not associated and drops
//! `details` (`:1567-1569`). KHook now has no record of the hook; nothing was patched for it and
//! nothing will call into its context, but the context, its reference and this module's `live`
//! count are never released.
//!
//! The delete thread loses an asynchronous removal the same way: [`Drop`]'s `RemoveHook(id,
//! async)` finds `id` associated and queues it (`K:1865-1867`); if the insert thread has popped
//! `id` and not yet inserted it, the delete thread's `__RemoveHook_Sync(id, false)` (`K:1644`)
//! takes the same not-found exit. Here nobody is waiting on a return, so nothing detects it on
//! the spot: the id stays in `tracked`/`pending` and `live` stays raised until
//! [`remove_all_blocking`] re-issues the removal synchronously and reconciles it.
//!
//! **When the window opens.** An id reaches the insert thread whenever it is queued:
//! every asynchronous registration on an address that already has a capsule (`K:1745-1748`),
//! and a synchronous one whose direct `__InsertHook_Sync` failed because the capsule's `try_lock`
//! failed (`K:1739-1741`, `:1453`: a call is in flight). The first registration on a fresh
//! address is forced synchronous (`K:1716-1717`), but it can fail the same way: the function is
//! already patched (`K:1705`) and the capsule lock released (`:1710`) before the insert, so a call
//! can be in flight. So for this module's synchronous setups the window needs a hooked call in
//! flight during setup; for asynchronous ones (and for other
//! plugins' hooks on a shared target, which we do not track) it needs no call at all. For the
//! bridge's targets: the save/restore config and the input slots are called and set up on the
//! main thread (in practice never); `StartSoundEvent` is called from the main and the sound
//! threads (possible).
//!
//! **A variant re-queues a stale entry.** If the capsule's `try_lock` fails in `InsertHook`
//! (`K:1453`, a call is in flight), `__InsertHook_Sync` returns false and the insert thread
//! pushes `(id, details)` back (`K:1618-1622`) after relocking `insert_hooks_mutex`, so a removal
//! can erase the association in between. The entry then sits at the end of `insert_hooks`: the
//! thread pops one entry per pass and sleeps ~5 ms between passes (`K:1607-1610`, `:1626`), so it
//! lives for (entries ahead of it + 1) passes until a pass drops it (not associated,
//! `:1566-1569`; dropping calls nothing). A `RemoveHook(id)` in that time takes the scan path and
//! **calls our removal function**, for a hook whose removal we already answered for. Unlike the
//! plain race, this variant needs a call in flight.
//!
//! ## Why a mutex alone cannot close it
//!
//! The window lies on KHook's insert thread, between two of KHook's own locks; no lock this
//! module holds is involved there, and KHook exposes no "is this id inserted" query (`IKHook`,
//! `khook.hpp:2184-2201`, has only the `Find*` lookups). Our own mutex still matters twice: it
//! keeps our setups and removals from overlapping each other (G1), and the bookkeeping that
//! decides "removed" versus "lost" is one critical section, because a stale entry's call into
//! our removal function can run on another thread at the same moment as our reconcile (G2). So
//! the guard is a mutex that makes the outcome *observable* plus deterministic detection and
//! repair.
//!
//! ## G1: one gate for every KHook call this module makes
//!
//! `KHOOK_GATE` is held around every `SetupHook`, `SetupVirtualHook` and `RemoveHook` this module
//! issues, **except** the asynchronous `RemoveHook` of a [`Drop`] that runs inside one of our
//! thunks ([`in_thunk`]). It guarantees: (i) no `RemoveHook(id)` for an id whose `SetupHook` has
//! not returned (so `id` is in `tracked` before anyone can remove it), and (ii) no two
//! *synchronous* removals of ours for the same id overlap, so "still tracked after my synchronous
//! removal returned" is never another synchronous removal of ours still in progress. (An
//! asynchronous removal on the delete thread can be in progress; the proof below covers it.)
//! Lock order is gate -> registry. Thunks and the removal function never wait for the gate, so
//! KHook calling our removal function inside `RemoveHook` cannot deadlock against it.
//!
//! The in-thunk exception is what keeps the gate deadlock-free: a thread inside a hooked call
//! holds that capsule shared, and a gated synchronous removal on another thread can be spinning
//! on that capsule's `try_lock` (`K:1503`, retry `:1596`) while it holds the gate; a callback
//! that then waited on the gate would never return. An asynchronous `RemoveHook` never waits on a
//! capsule (`K:1852-1869`), and it is serialised with our scans by `insert_hooks_mutex` and with
//! our capsule removals by `associated_hooks_mutex`, so it needs no gate. A **registration** from
//! inside a thunk is the same shape of call: its `SetupHook` takes `hooks_detour_mutex`
//! **exclusive** for a fresh address (to create the capsule, `K:1698-1710`) and **shared** across
//! the insert (`K:1722-1750`), takes `hook_id_mutex` (`K:1729`) and `associated_hooks_mutex`
//! (`K:1734`) briefly and `insert_hooks_mutex` (`K:1746`), and the capsule insert is a `try_lock`
//! (`K:1453`) — a registration on the thread's own target just goes to the insert thread. No
//! holder of `hook_id_mutex`, `associated_hooks_mutex` or `insert_hooks_mutex` ever waits on a
//! capsule or on the gate (`__RemoveHook_Sync` holds `associated_hooks_mutex` only around a
//! capsule `try_lock` and sleeps outside it, `K:1578-1598`); `hooks_detour_mutex` is the one
//! exception, in the two destructor runs (3)/(4) above: `Shutdown` clears the tables
//! (`K:1885-1888`) and the fresh-address race's second `insert_or_assign` (`K:1701`) destroys the
//! capsule it replaced, and the destructor then blocks on `_detour_mutex.lock()` (`K:1427`) while
//! still holding `hooks_detour_mutex`. That wait can close on *this* thread only when a foreign
//! plugin loses a first-registration race on the address this thread is in flight on, while our
//! thunk registers: Y holds `hooks_detour_mutex` exclusive waiting at `K:1427` for T; T's
//! registration waits at `K:1695/1722`. Such a deadlock requires the KHook race that already
//! destroys hooks — the same one the [`remove_all_blocking`] barrier comment puts outside this
//! module's control — and `Shutdown`, which runs on the game thread and cannot overlap the
//! GameFrame tick a thunk registration of ours runs in. So a registration from a thunk is
//! deadlock-free provided it never blocks on the gate: it takes `KHOOK_GATE` with `try_lock` and
//! answers the new [`Error::GateBusy`] when the gate is held (the caller retries on a later
//! tick); outside a thunk it still waits for the gate.
//!
//! ## G2: detect after a synchronous removal, then reconcile
//!
//! `tracked` holds every id KHook accepted and whose removal function has not run;
//! [`release_on_removal`] takes the id out. **Claim:** when a synchronous `RemoveHook(id)` issued
//! under the gate has returned, either the removal function for `id` has run and fully retired,
//! or KHook no longer holds `id` and will not run it, except through a stale re-queued entry.
//!
//! *Proof.* `RemoveHook(id, async = false)` first runs the scan under `insert_hooks_mutex`: if
//! `id` is in the list, the removal function runs right there, inside our call. Otherwise
//! `__RemoveHook_Sync(id, true)` loops until either `id` is not associated (return) or the
//! capsule's `RemoveHook` succeeds under `associated_hooks_mutex` exclusive; when the capsule
//! has the callback, the removal function runs inside that, again inside our call. Any removal
//! function call already running on another thread (another plugin's scan, the delete thread's
//! `__RemoveHook_Sync(id, false)`, `K:1644`) is inside call site 1 or 2 and holds that site's
//! mutex for its whole duration, and our call takes both mutexes in turn, so it cannot return
//! while such a call is still inside our removal function. What remains when it returns with
//! `id` still in `tracked`: no path ran the removal function, the id is not associated (both
//! exits of `__RemoveHook_Sync` leave it so), and it is not in `insert_hooks` as of the scan.
//! The only way back to a removal-function call is an entry the insert thread re-queues
//! afterwards (the variant above), which is never inserted again, because inserting needs the
//! association (`K:1567-1569`) and ids are never reused (`K:1729`). ∎
//!
//! **Reconcile.** So a synchronous removal that returns with `id` still tracked means KHook lost
//! it: under one registry critical section the id leaves `tracked`/`installed`/`pending`, `live`
//! is lowered and the id goes into `lost`; then (outside the lock) the diagnostic sink gets
//! `khook: lost removal for id N reconciled (KHook insert race)`. The context is **leaked**:
//! neither KHook's reference nor the [`Hook`]'s is ever released (a later `Drop` or
//! [`Hook::remove_sync`] of a lost id releases nothing and asks KHook nothing).
//!
//! Every call of our removal function is admitted by id, in one registry critical section,
//! before anything else happens (`admit_removal`): if `id` is in `lost` or in `removal_fired` the
//! call is ignored; otherwise `id` goes into `removal_fired` and is claimed from `tracked`, or, if
//! it is in neither (the registration window above), noted for the registration to find. Only an
//! admitted call then fetches the context, releases KHook's reference and lowers `live`. Because
//! the check and the claim are one critical section, a stale call that straddles a reconcile on
//! another thread (it checks before the reconcile and would claim after it) cannot exist: either
//! the reconcile takes `id` out of `tracked` first and the call is ignored, or the call claims
//! first and the reconcile finds nothing to reconcile. `r3_stale_call_straddling_the_reconcile_is_ignored`
//! pins that interleaving; with the section split in two it double-counts. `lost` and
//! `removal_fired` are never cleared: ids are never reused, so they cost a few bytes per hook
//! ever removed.
//!
//! **Why the leak and the `lost` check.** The stale entry carries our removal function and the
//! context pointer. When a later scan calls it, [`release_on_removal`] finds `id` in `lost`
//! before it touches anything and returns. KHook passes the context only as the "current hook"
//! pointer (`K:1840`), which that check never reads, so the stale call never dereferences it;
//! still, freeing the context at reconcile time
//! would leave KHook holding a dangling pointer in `details`, and running the normal path would
//! release a reference twice and lower `live` for a hook already accounted for. The leak is
//! defence in depth; the `lost` check is required.
//!
//! **The drain.** [`remove_all_blocking`] re-issues one synchronous `RemoveHook` for every lost id
//! on every call (when a KHook API is still installed; only ids drained for the first time are
//! reported): a stale entry still queued is
//! consumed by the scan and our removal function ignores it; otherwise the call is a no-op
//! ("not associated", `K:1583-1586`). This is defence in depth, not a closed window. After our
//! `Unload`, Metamod's `Unloader` (`MM: core/metamod_plugins.cpp:46-56`, built in `~CPlugin`,
//! `:205-213`) issues an asynchronous `RemoveHook` for every id the plugin ever registered (its
//! `m_hooks` is never pruned, `core/metamod_khook.h:94-96`), and `dlclose`s the image only after
//! every one of those has reported back (`:62-88`); those scans consume any stale entry that is
//! in the list at that point, calling our removal function after this module's KHook API was
//! uninstalled. That is why the removal function decides by id alone, before it touches the API
//! or the context, and never panics: a lost or already-fired id is answered without either. A
//! tracked id whose removal arrives with no API installed (or a null context from KHook) is
//! accounted for (`live` lowered, `unbarriered_removal` set) and its context leaked, with a
//! diagnostic line. What neither closes is an entry the insert thread holds popped at
//! that moment and re-queues after relocking `insert_hooks_mutex` (`K:1618-1622`): it lands after
//! the `dlclose`, and the next pass drops it without calling it (not associated). It only
//! matters if something issues `RemoveHook` for that id after the unload, which nothing does.
//! Documented for KHook upstream in `workflows/upstream-prs/khook-lost-removal.md`.
//!
//! ## What callers must still do
//!
//! * Remove synchronously ([`Hook::remove_sync`]) from ordinary code, and never remove a target
//!   synchronously while it may be on some thread's call stack: `__RemoveHook_Sync(id, true)` has
//!   no deadline and spins on a capsule its own caller holds shared (`K:1578-1598`). Inside one
//!   of our thunks (pre, post or `make_call_original`; [`in_thunk`] counts all three)
//!   `remove_sync` does not spin, in any build: [`in_thunk`] cannot tell "the target is on this
//!   stack" (a caller bug) from "a legitimate op running in another hook's tick" (the bridge's
//!   GameFrame tick), and an asynchronous `RemoveHook` never waits on a capsule or on the gate,
//!   so falling back to the asynchronous [`Drop`] path and answering [`Removal::Deferred`] is
//!   always safe. With no API installed it answers
//!   [`Error::KHookNotInstalled`] and forgets the [`Hook`]. [`remove_all_blocking`] refuses with
//!   [`Error::InThunk`] in every build.
//!   [`in_thunk`] only sees this module's thunks: a call stack under another plugin's hook, or
//!   under the hooked function itself without one of our callbacks on it, is the caller's rule
//!   (a per-frame hook removes itself asynchronously, through [`Drop`]).
//! * [`Drop`] stays asynchronous for generic users. Its removal is made under the gate except
//!   inside a thunk. A `Drop` whose asynchronous removal the delete thread lost keeps
//!   [`live_context_count`] above zero until [`remove_all_blocking`] re-issues it synchronously
//!   and reconciles it, so call that before an unload. A `Drop` of a lost id releases nothing and
//!   asks KHook nothing; a `Drop` with no API installed releases only the `Hook`'s own reference
//!   and leaves KHook's registration (and reference) behind, with a diagnostic line.
//! * Uninstall the API ([`uninstall`]) only after [`remove_all_blocking`] answered `Ok`. A thunk
//!   entered while no API is installed cannot run KHook's protocol: it returns the type's default
//!   without calling the original, and its `make_return` cannot call `DestroyReturnValue`, so KHook
//!   keeps that capsule locked shared and its saved-parameter stack pushed on that thread
//!   (`DestroyReturnValue`, `K:408-433`; `khook.hpp:779-788`): later synchronous removals on
//!   that capsule spin, its destructor blocks (`K:1427`), and the thread's next detour fails its
//!   stack check (`K:172-175`).
//!   That state is a broken process, not a fallback; the module only avoids crashing in it.

use core::cell::Cell;
use core::ffi::{c_char, c_void};
use core::mem::size_of;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, MutexGuard, RwLock};
use std::time::Duration;

use thiserror::Error as ThisError;

// ---------------------------------------------------------------------------------------
// Action / Return / HookId (khook.hpp:41-63, :115-116)
// ---------------------------------------------------------------------------------------

/// `KHook::Action` (`khook.hpp:41-52`): what a callback tells KHook to do with the call.
///
/// The order matters: `Ignore < Override < Supersede`, and a thunk folds several callbacks'
/// answers by taking the maximum (`khook.hpp:744-752`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// The hook has taken no specific action (KHook's `Ignore = 0`).
    Ignore = 0,
    /// The hook overwrote the return value; the original is still called from a `pre`
    /// callback, and the action does nothing in a `post` (`khook.hpp:44-47`).
    Override = 1,
    /// The hook overwrote the return value and the original must not be called from a `pre`
    /// callback (`khook.hpp:48-51`).
    Supersede = 2,
}

/// `KHook::Return<R>` (`khook.hpp:54-58`): a callback's action plus the value it wants used.
///
/// A void hook uses `Return<()>`; its value carries no information (and `SaveReturnValue` is
/// given size 0 for it), which is as close as a Rust type comes to the header's
/// `Return<void>` specialization (`khook.hpp:60-63`, action only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Return<R> {
    /// What KHook should do with this answer.
    pub action: Action,
    /// The value to use when `action` wins the fold.
    pub value: R,
}

impl<R> Return<R> {
    /// An `Ignore` answer carrying `value` (used as the fold's seed; KHook never reads it).
    #[must_use]
    pub const fn ignore(value: R) -> Self {
        Self {
            action: Action::Ignore,
            value,
        }
    }

    /// An `Override` answer: use `value` but still call the original.
    #[must_use]
    pub const fn override_with(value: R) -> Self {
        Self {
            action: Action::Override,
            value,
        }
    }

    /// A `Supersede` answer: use `value` and do not call the original.
    #[must_use]
    pub const fn supersede_with(value: R) -> Self {
        Self {
            action: Action::Supersede,
            value,
        }
    }
}

/// `KHook::HookID_t` (`khook.hpp:115`).
pub type HookId = u32;

/// `KHook::INVALID_HOOK` (`khook.hpp:116`): what a failed registration returns.
pub const INVALID_HOOK: HookId = u32::MAX;

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// What can go wrong setting a hook up.
#[derive(Debug, ThisError)]
pub enum Error {
    /// No `IKHook` was installed in this process, so no hook can be registered. The plugin
    /// must call [`install`] with the pointer `ISmmAPI::GetDetourInterface` handed it.
    #[error("no KHook IKHook API is installed (call s2sdk::hook::install first)")]
    KHookNotInstalled,

    /// KHook refused the registration and returned [`INVALID_HOOK`] (`khook.hpp:2206-2231`:
    /// a hook set up before Metamod's KHook is up, or a `SetupVirtualHook` it cannot honour).
    /// The wrapper never returns a live [`Hook`] for this.
    #[error(
        "{message}",
        message = if *virtual_hook {
            "KHook::SetupVirtualHook returned INVALID_HOOK (0xFFFFFFFF)"
        } else {
            "KHook::SetupHook returned INVALID_HOOK (0xFFFFFFFF)"
        }
    )]
    HookSetupFailed {
        /// Whether the refused call was [`KHookApi::setup_virtual_hook`].
        virtual_hook: bool,
    },

    /// A stack size declared by the caller (e.g. measured from generated IR) disagrees with
    /// the one computed from the signature. KHook allocates its copy stack from this number
    /// (`detour.cpp:794`), so a mismatch means the trampoline would copy the wrong amount of
    /// stack: it is refused instead.
    #[error("declared stack size {declared} does not match the computed stack size {computed}")]
    StackSizeMismatch {
        /// The caller's declared value.
        declared: u32,
        /// The value the signature computes to.
        computed: u32,
    },

    /// A registration whose signature needs more copy-stack bytes than the `DetourCapsule`
    /// already covering that address was created with. KHook fixes a hooked address's copy
    /// stack on the first registration on it and ignores the argument afterwards
    /// (`detour.cpp:1696-1719`, rounded up to 16 at `:536`), so a later one that needs more
    /// would be run against a stack that is too small — the dangerous direction. It is refused
    /// here; a later registration that needs less is fine.
    #[error(
        "the hook capsule for address {address:#x} was created for {capsule} bytes of hook \
         stack, which is less than the {needed} bytes this registration needs: KHook fixes the \
         capsule's stack size on the first registration on an address and ignores it afterwards"
    )]
    CapsuleStackTooSmall {
        /// The hooked address KHook identifies the capsule by (the function pointer for
        /// `SetupHook`, the vtable slot's own address for `SetupVirtualHook`).
        address: usize,
        /// The rounded stack size the existing capsule was created with.
        capsule: u32,
        /// The rounded stack size this signature needs.
        needed: u32,
    },

    /// [`remove_all_blocking`] re-issued every removal this module knew about and KHook still
    /// had not run the removal function of at least one of them, so its count of contexts KHook
    /// may still call into did not reach zero. Unloading now is not proven safe: either a
    /// removal function of this module is still to come, or this module counted a context KHook
    /// no longer holds and will never run it for.
    #[error(
        "{remaining} hook context(s) are still reachable from KHook after the unload deadline: \
         stopping here rather than unloading while a removal function of this module can still run"
    )]
    UnloadTimeout {
        /// How many contexts were still live when the deadline passed.
        remaining: usize,
    },

    /// A hook operation that is made while this thread is inside a hook thunk of this module
    /// ([`in_thunk`]) was refused. Today that is only [`remove_all_blocking`]: [`Hook::remove_sync`]
    /// hands its removal to the asynchronous path [`Drop`] takes instead ([`Removal::Deferred`]),
    /// so only the drain, which is nothing but synchronous removals, is refused.
    ///
    /// A hooked call holds its address's mutex **shared** for its whole duration
    /// (`detour.cpp:214`) and every **synchronous removal** of this module waits for that mutex
    /// with no deadline (`detour.cpp:1503,1581-1598`), so from a thunk a synchronous removal
    /// either spins for ever on its own stack or — because this module serialises its
    /// setup/removal calls behind [`KHOOK_GATE`] — deadlocks against a gate holder that is
    /// already spinning on this call's capsule. A drain is nothing but synchronous removals,
    /// so it is refused outright. A **registration** from a thunk issues no synchronous
    /// removal and never waits on a capsule (its setup takes `hooks_detour_mutex` exclusive
    /// for a fresh address, `detour.cpp:1698-1710`, shared across the insert, `:1722-1750`,
    /// and its capsule insert is a `try_lock`, `:1453`; the one capsule wait on
    /// `hooks_detour_mutex` itself — a destructor run under `Shutdown` or the fresh-address
    /// `insert_or_assign` race, both outside this module's control — is the G1 exception in
    /// the module docs), so it is
    /// allowed: it takes `KHOOK_GATE` with `try_lock` and answers [`Error::GateBusy`] when
    /// the gate is held, rather than waiting on a holder that may be spinning on this call's
    /// capsule.
    #[error(
        "hook operation refused from inside a hook thunk: the hooked call on this stack holds \
         its address's mutex shared, and a synchronous KHook call of this module would wait for \
         it without a deadline (detour.cpp:214,1503,1581-1598)"
    )]
    InThunk,

    /// A registration made from inside a hook thunk ([`in_thunk`]) found the hook gate held
    /// and did not wait for it: from a thunk the gate is taken with `try_lock` only, because
    /// its holder may be in a synchronous removal that is spinning on the very capsule this
    /// call holds shared (`detour.cpp:214,1503,1596`), so a blocking wait here could deadlock
    /// ([`Error::InThunk`] documents the refusal behind that). Nothing was registered, no
    /// KHook call was made, and the context was freed exactly as on every other registration
    /// error — retry the registration on a later tick, or from outside the thunk.
    #[error(
        "a registration from inside a hook thunk found the KHook gate held and did not wait \
         for it: the holder may be spinning in a synchronous removal on this call's capsule \
         (detour.cpp:214,1503,1596); retry on a later tick"
    )]
    GateBusy,
}

/// Result alias for this module.
pub type Result<T> = core::result::Result<T, Error>;

// ---------------------------------------------------------------------------------------
// The IKHook trait
// ---------------------------------------------------------------------------------------

/// Metamod's `KHook::IKHook` (`khook.hpp:2184-2201`), one method per virtual, in header
/// order, with the header's `void*`-typed parameters kept as raw pointers: the thunks behind
/// them have the *hooked* function's signature, so no Rust type can describe them here.
///
/// Implementations must be usable for the whole process life of a hook (`Send + Sync`), the
/// way the single `IKHook` object KHook hands out is.
///
/// The vtable-backed implementation (`IKHook_vtable` from `s2sdk-sys`) is a later step; this
/// trait is what the wrapper and the generated thunks are written against so it can be swapped
/// in without touching them. `Action` is passed as this crate's `#[repr(u8)]` enum, which is
/// layout-identical to `KHook::Action` (`khook.hpp:41`); the vtable implementation may need to
/// convert it to whatever type the generated binding uses for that slot.
pub trait KHookApi: Send + Sync {
    /// `IKHook::SetupHook` (`khook.hpp:2186`): hook a function address.
    ///
    /// # Safety
    ///
    /// `function` must be a live function address and every pointer must have the signature
    /// and ABI KHook will call it with; `context` must stay alive until the removal function
    /// passed as `removed_function` has run.
    // The header's own signature; the count is the point, not a design smell.
    #[allow(clippy::too_many_arguments)]
    unsafe fn setup_hook(
        &self,
        function: *mut c_void,
        context: *mut c_void,
        removed_function: *mut c_void,
        pre: *mut c_void,
        post: *mut c_void,
        make_return: *mut c_void,
        make_call_original: *mut c_void,
        stack_size: u32,
        r#async: bool,
    ) -> HookId;

    /// `IKHook::SetupVirtualHook` (`khook.hpp:2187`): hook one entry of a vtable.
    ///
    /// # Safety
    ///
    /// As [`KHookApi::setup_hook`], plus `vtable` must point at a live vtable of at least
    /// `index + 1` entries.
    #[allow(clippy::too_many_arguments)]
    unsafe fn setup_virtual_hook(
        &self,
        vtable: *mut *mut c_void,
        index: i32,
        context: *mut c_void,
        removed_function: *mut c_void,
        pre: *mut c_void,
        post: *mut c_void,
        make_return: *mut c_void,
        make_call_original: *mut c_void,
        stack_size: u32,
        r#async: bool,
    ) -> HookId;

    /// `IKHook::RemoveHook` (`khook.hpp:2188`). `hook_removal_fn` is the *caller-supplied*
    /// callback (called with `context`); the registered `removed_function` is invoked by KHook
    /// itself either way (`detour.cpp:1819-1877`). This wrapper passes null for both.
    ///
    /// # Safety
    ///
    /// `id` must be a live hook id; `hook_removal_fn` and `context` follow the header's
    /// contract.
    unsafe fn remove_hook(
        &self,
        id: HookId,
        r#async: bool,
        hook_removal_fn: *mut c_void,
        context: *mut c_void,
    );

    /// `IKHook::GetContextPtr` (`khook.hpp:2189`): the `context` of the hook currently
    /// running on this thread. Only meaningful inside a thunk.
    fn get_context_ptr(&self) -> *mut c_void;

    /// `IKHook::GetOriginalFunction` (`khook.hpp:2190`): the unhooked function.
    fn get_original_function(&self) -> *mut c_void;

    /// `IKHook::GetOriginalValuePtr` (`khook.hpp:2191`): the original call's saved value.
    fn get_original_value_ptr(&self) -> *mut c_void;

    /// `IKHook::GetOverrideValuePtr` (`khook.hpp:2192`): the override's saved value.
    fn get_override_value_ptr(&self) -> *mut c_void;

    /// `IKHook::GetCurrentValuePtr` (`khook.hpp:2193`): the value that will be used;
    /// `pop` selects the finished call's snapshot over the running one.
    fn get_current_value_ptr(&self, pop: bool) -> *mut c_void;

    /// `IKHook::DestroyReturnValue` (`khook.hpp:2194`): free the current call's saved values.
    fn destroy_return_value(&self);

    /// `IKHook::FindOriginal` (`khook.hpp:2195`).
    fn find_original(&self, function: *mut c_void) -> *mut c_void;

    /// `IKHook::FindOriginalVirtual` (`khook.hpp:2196`).
    fn find_original_virtual(&self, vtable: *mut *mut c_void, index: i32) -> *mut c_void;

    /// `IKHook::DoRecall` (`khook.hpp:2197`): re-enter the hook with new arguments.
    ///
    /// # Safety
    ///
    /// Only valid inside a thunk, with the same value/operator contract as
    /// [`KHookApi::save_return_value`].
    unsafe fn do_recall(
        &self,
        action: Action,
        ptr_to_return: *mut c_void,
        return_size: usize,
        init_op: *mut c_void,
        deinit_op: *mut c_void,
    ) -> *mut c_void;

    /// `IKHook::SaveReturnValue` (`khook.hpp:2198`): store the return value for the running
    /// call (`original = false` from `pre`/`post`, `true` from `make_call_original`).
    ///
    /// # Safety
    ///
    /// Only valid inside a thunk. `ptr_to_return` must be readable for `return_size` bytes
    /// (ignored when `return_size` is 0, as `detour.cpp:373-380,397-404` guards); `init_op`
    /// and `deinit_op` must be the corresponding `void(T* dst, T* src)` / `void(T*)`.
    unsafe fn save_return_value(
        &self,
        action: Action,
        ptr_to_return: *mut c_void,
        return_size: usize,
        init_op: *mut c_void,
        deinit_op: *mut c_void,
        original: bool,
    );

    /// `IKHook::LookupSignature` (`khook.hpp:2199`): IDA-style byte pattern search that sees
    /// through KHook's own detours.
    fn lookup_signature(&self, start: *mut c_void, size: usize, signature: *const c_char)
        -> *mut c_void;

    /// `IKHook::WasOriginalFunctionSkipped` (`khook.hpp:2200`): did this call skip the
    /// original (`detour.cpp:470-472`, the `CompleteSkipped` state a `Supersede` sets).
    fn was_original_function_skipped(&self) -> bool;
}

// ---------------------------------------------------------------------------------------
// The process-wide API slot
// ---------------------------------------------------------------------------------------

/// The `IKHook` every thunk talks to. Set once at plugin load ([`install`]); the lock is only
/// written there and by [`uninstall`], and read (uncontended) by the thunks.
static API: RwLock<Option<&'static dyn KHookApi>> = RwLock::new(None);

/// Install the process's `IKHook` (the pointer `ISmmAPI::GetDetourInterface` returned). All
/// hooks registered through this module talk to this object.
///
/// Replaces a previous value, which is only useful in tests: a plugin installs once, before
/// it sets up any hook.
///
/// The set of ids whose removals were reconciled as lost (`Registry::lost`) is **not**
/// cleared, here or in [`uninstall`]: KHook's hook-id counter is one global of the library
/// (`detour.cpp:1546-1548`, incremented under `hook_id_mutex` at `:1728-1729`), not a counter
/// per `IKHook`, and Metamod's per-plugin `KHookImpl` forwards every call to that same library
/// (`crates/metamod/upstream/core/metamod_khook.h:49,77`). An id is therefore never reused in
/// the process, whichever object hands it out, and a new `IKHook` cannot hand out an id an
/// older one lost.
pub fn install(api: &'static dyn KHookApi) {
    *API.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(api);
}

/// Remove the installed `IKHook`.
///
/// **Never call this while a [`Hook`] is alive**: [`Drop`] needs the API to remove the hook, and
/// without one it reports the registration as leaked (KHook keeps the hook and its reference;
/// the `Drop` releases only its own). A removal function KHook calls later answers the same way
/// (diagnostic + leak), because a panic inside an `extern "C"` function aborts the process
/// instead of unwinding. It exists so a test can swap one mock for another between cases.
///
/// The reconciled-lost ids stay (`Registry::lost`), and so does the record of every id whose
/// removal function already ran (`Registry::removal_fired`): KHook's id counter belongs to its
/// library, not to one `IKHook` (`detour.cpp:1546-1548,1728-1729`), so ids are not reused after
/// this either and forgetting them would only let a stale removal-function call touch a context
/// this module gave up on. [`lost_removal_count`] likewise keeps its process-wide value.
pub fn uninstall() {
    API.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

/// The installed `IKHook`, if any.
#[must_use]
pub fn api() -> Option<&'static dyn KHookApi> {
    *API.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// [`api`] for paths that want to name the missing API in a panic.
///
/// **The generated thunks and the removal path do not use it**: a panic inside an `extern "C"`
/// function is a non-unwinding abort (the game dies), so everything KHook can enter
/// ([`HookCall`]'s queries, `save_value`, `make_call_original`, `make_return`, the removal
/// function, [`Drop`]) takes [`api`] and answers with a diagnostic line and the type's default
/// value instead. That state is not a fallback: a thunk entered with no API cannot call
/// `DestroyReturnValue`, so KHook keeps its capsule's `_detour_mutex` shared and the saved-params
/// stack pushed on that thread (`detour.cpp:408-433`, `khook.hpp:779-788`); later synchronous
/// removals on that capsule spin, its destructor blocks (`detour.cpp:1427`), and the thread's
/// next detour fails the stack check (`:172-175`). Broken process; this module only avoids
/// crashing in it.
#[doc(hidden)]
#[must_use]
pub fn api_expect() -> &'static dyn KHookApi {
    api().expect("no KHook API installed: s2sdk::hook::install(ik_hook) must run before any hook")
}

/// Set once, so a hooked function that is called in a tight loop with no API installed reports
/// the condition once instead of flooding the sink.
static THUNK_WITHOUT_API_REPORTED: AtomicBool = AtomicBool::new(false);

/// Report that a thunk of this module ran with no API installed (or with no context on this
/// thread).
///
/// A missing API is a broken process (a plugin uninstalled the API while a hook was live), not a
/// fallback. No callback is dispatched, no original is called, and the type's default value is
/// returned — but a thunk entered with no API cannot call `DestroyReturnValue`, so KHook keeps
/// its capsule's `_detour_mutex` shared and the saved-params stack pushed on that thread
/// (`detour.cpp:408-433`, `khook.hpp:779-788`); later synchronous removals on that capsule spin,
/// its destructor blocks (`detour.cpp:1427`), and the thread's next detour fails the stack check
/// (`:172-175`). Broken process; this module only avoids crashing in it. The alternative — a
/// panic — would abort the game from inside KHook's trampoline. Reported once per process; the
/// diagnostic sink may not call back into this module (see [`set_diagnostic_sink`]).
///
/// A null context with the API installed is a broken `IKHook` instead (`GetContextPtr` reads the
/// thread's current hook, which KHook sets around every thunk, `detour.cpp:359-361`): same
/// answer, reported with its own line.
#[doc(hidden)]
pub fn report_thunk_without_api(what: &str) {
    if !THUNK_WITHOUT_API_REPORTED.swap(true, Ordering::Relaxed) {
        if api().is_none() {
            emit_diagnostic(&format!(
                "khook: {what} ran with no KHook IKHook installed; the call is not dispatched \
                 (the API was uninstalled while a hook was live)"
            ));
        } else {
            emit_diagnostic(&format!(
                "khook: {what} ran with no context on this thread (GetContextPtr answered \
                 null); the call is not dispatched"
            ));
        }
    }
}

/// The entry point `make_call_original` must call (`GetOriginalFunction`), if there is one.
///
/// `None` — and never a panic — when the API is gone (reported through
/// [`report_thunk_without_api`]) or KHook answers null (a broken `IKHook`, made visible by the
/// debug assert; the release build returns `None` so the thunk can answer the default value).
#[doc(hidden)]
#[must_use]
pub fn original_function_ptr() -> Option<*mut c_void> {
    let Some(api) = api() else {
        report_thunk_without_api("make_call_original");
        return None;
    };
    let function = api.get_original_function();
    if function.is_null() {
        // Inside a thunk KHook has the running capsule (`g_current_hook`, `detour.cpp:359-361`),
        // so a null here means a broken KHook. Debug builds abort on this assert; release builds
        // must not abort the game from a thunk.
        debug_assert!(false, "KHook has no original function for this call");
        return None;
    }
    Some(function)
}

// ---------------------------------------------------------------------------------------
// Read-only queries: the pre-hook entry of a function or vtable slot
// ---------------------------------------------------------------------------------------

/// `IKHook::FindOriginal` (`khook.hpp:2195`, `detour.cpp:1897-1905`): the entry KHook replaced
/// on `function`, or `function` itself when it is not hooked — so the answer is useful either
/// way, and it sees through KHook's detours even when the caller saved no original pointer.
///
/// `None` only when no API is installed: there is nobody to ask.
///
/// Read-only for KHook (a shared lock on `hooks_detour`, `detour.cpp:1899`), so it takes no
/// [`KHOOK_GATE`] and may be called from the verifier thread, a hook callback, or anywhere else.
#[must_use]
pub fn find_original(function: *mut c_void) -> Option<*mut c_void> {
    api().map(|api| api.find_original(function))
}

/// `IKHook::FindOriginalVirtual` (`khook.hpp:2196`, `detour.cpp:1907-1915`): the entry KHook
/// replaced on `vtable[index]`, or the slot's current entry when it is not hooked.
///
/// `None` only when no API is installed. Read-only for KHook, like [`find_original`], so no
/// [`KHOOK_GATE`].
///
/// # Safety
///
/// `vtable` must point at a live vtable of at least `index + 1` entries (`index` may be
/// negative, in which case the slot is before `vtable`): KHook computes the slot with the C++
/// pointer arithmetic `vtable + index` (`detour.cpp:1797,1909`) and, for an unhooked slot,
/// answers by reading it (`detour.cpp:1914`). In Rust terms the contract is that
/// `vtable.offset(index)` is a readable `*mut c_void`.
#[must_use]
pub unsafe fn find_original_virtual(vtable: *mut *mut c_void, index: i32) -> Option<*mut c_void> {
    api().map(|api| api.find_original_virtual(vtable, index))
}

// ---------------------------------------------------------------------------------------
// The KHook gate: one setup/removal at a time
// ---------------------------------------------------------------------------------------

/// The lock every `SetupHook`/`SetupVirtualHook`/`RemoveHook` call of this module is made
/// under, plus the two registration halves (the capsule claim, the registry bookkeeping) that
/// bracket it.
///
/// It is not KHook's lock: KHook runs its own insert thread, and the window this gate narrows
/// is between two KHook locks *on that thread* (`detour.cpp:1603-1628` pops an entry under
/// `insert_hooks_mutex`, releases it, and only then takes `associated_hooks_mutex` in
/// `__InsertHook_Sync`). No lock of ours can close that window; the gate is the G1
/// precondition that makes our own observations about it meaningful — one registration or
/// removal of this module is in KHook at a time, so a synchronous `RemoveHook` that returns
/// with an id still in [`Registry::tracked`] really did lose it, rather than having been
/// observed halfway through somebody else's setup (see [`reconcile_lost`]).
///
/// **Lock order is `KHOOK_GATE -> REGISTRY`, never the reverse.** A removal function of this
/// module runs inside KHook's `RemoveHook` (called by us while we hold the gate) and takes
/// [`REGISTRY`], so anything that took `REGISTRY` and then waited for the gate would deadlock
/// against a removal KHook runs inside that call. The thunks and [`release_on_removal`] never
/// wait for the gate, for the same reason.
static KHOOK_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`KHOOK_GATE`], recovering from a poisoned lock the way [`registry`] does: a panic
/// while a gate holder was in KHook must not wedge every later hook operation of the process.
fn khook_gate() -> MutexGuard<'static, ()> {
    KHOOK_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Take [`KHOOK_GATE`] only if it is free — the `try_lock` a registration from inside a thunk
/// makes (see [`Error::GateBusy`]). A poisoned gate is recoverable exactly as [`khook_gate`]
/// recovers: the guard is in the error, and the panic it stands for was some earlier gate
/// holder's, not this caller's.
fn khook_gate_try() -> Option<MutexGuard<'static, ()>> {
    match KHOOK_GATE.try_lock() {
        Ok(gate) => Some(gate),
        Err(std::sync::TryLockError::Poisoned(err)) => Some(err.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

// ---------------------------------------------------------------------------------------
// Callback context object
// ---------------------------------------------------------------------------------------

/// What a `pre`/`post` callback is given: the hooked call (`this`) and the `IKHook` queries
/// KHook documents as valid inside a thunk (`khook.hpp:2171-2184`).
///
/// Valid only for the duration of the callback: every pointer it returns points into KHook's
/// per-call storage, which [`destroy_return_value`](KHookApi::destroy_return_value) frees.
#[derive(Clone, Copy, Debug)]
pub struct HookCall {
    this: *mut c_void,
}

impl HookCall {
    /// Wrap the `this` the trampoline passed (`khook.hpp:759-765`).
    #[doc(hidden)]
    #[must_use]
    pub fn new(this: *mut c_void) -> Self {
        Self { this }
    }

    /// The hooked object/`this` the trampoline passed, exactly as the hooked function saw it.
    #[must_use]
    pub fn this(&self) -> *mut c_void {
        self.this
    }

    /// `GetOriginalValuePtr`: the value the original call produced, if it ran. Null when no
    /// original ran — a `pre` that superseded the call leaves nothing here, because the
    /// trampoline skips the original entirely (`detour.cpp:889-893`).
    ///
    /// Null too when no API is installed: a callback only runs while one is (KHook called a
    /// thunk of this module), and a query must not panic inside KHook's trampoline if that
    /// invariant is ever broken — a panic in an `extern "C"` thunk aborts the process.
    #[must_use]
    pub fn original_value_ptr(&self) -> *mut c_void {
        api().map_or(ptr::null_mut(), |api| api.get_original_value_ptr())
    }

    /// `GetOverrideValuePtr`: the value an `Override`/`Supersede` callback stored. Null without
    /// an API, as [`HookCall::original_value_ptr`] documents.
    #[must_use]
    pub fn override_value_ptr(&self) -> *mut c_void {
        api().map_or(ptr::null_mut(), |api| api.get_override_value_ptr())
    }

    /// `GetCurrentValuePtr(false)` (`detour.cpp:453-458`): the value that will be used for the
    /// call this callback is part of — the override when an `Override`/`Supersede` stored one,
    /// the original's otherwise.
    ///
    /// The header's `pop = true` is not offered here: it reads `g_last_loop`
    /// (`detour.cpp:454-458`, `KHook::GetCurrentValuePtr`), the *previous finished* call's
    /// snapshot of the same thread (`detour.cpp:432` keeps it), which is KHook's own business
    /// inside `make_return` (the generated thunk calls the trait's
    /// [`get_current_value_ptr(true)`](KHookApi::get_current_value_ptr)). A user callback wants
    /// the running call.
    #[must_use]
    pub fn current_value_ptr(&self) -> *mut c_void {
        api().map_or(ptr::null_mut(), |api| api.get_current_value_ptr(false))
    }

    /// `GetOriginalFunction`: the unhooked function, so a callback can call it itself. Null
    /// without an API, as [`HookCall::original_value_ptr`] documents.
    #[must_use]
    pub fn original_function(&self) -> *mut c_void {
        api().map_or(ptr::null_mut(), |api| api.get_original_function())
    }

    /// `WasOriginalFunctionSkipped`: true when a `pre` callback superseded the call. False
    /// without an API, as [`HookCall::original_value_ptr`] documents.
    #[must_use]
    pub fn was_original_function_skipped(&self) -> bool {
        api().is_some_and(|api| api.was_original_function_skipped())
    }
}

// ---------------------------------------------------------------------------------------
// Hook context header (shared by every generated context)
// ---------------------------------------------------------------------------------------

/// First field of every generated hook context: the reference count that keeps the context
/// alive exactly as long as KHook (until the removal function runs) or the [`Hook`] (until
/// `Drop` finished) may still use it, and the dropper for the concrete context type.
///
/// `#[repr(C)]` with this header first is what lets [`release`] find it from the opaque
/// `context` pointer KHook hands back.
#[repr(C)]
pub struct CtxHeader {
    refs: AtomicUsize,
    removed_fired: AtomicBool,
    drop_ctx: unsafe fn(*mut c_void),
}

impl CtxHeader {
    /// A header for a context allocation, with the initial reference held by its creator.
    #[must_use]
    pub fn new(drop_ctx: unsafe fn(*mut c_void)) -> Self {
        Self {
            refs: AtomicUsize::new(1),
            removed_fired: AtomicBool::new(false),
            drop_ctx,
        }
    }
}

/// Move `value` into an allocation and return it as the opaque `context` pointer
/// (`Box::into_raw`, so it is freed by [`release`] and never by Rust's scoping).
#[doc(hidden)]
#[must_use]
pub fn ctx_alloc<T>(value: T) -> *mut c_void {
    Box::into_raw(Box::new(value)).cast()
}

/// Drop a context allocated by [`ctx_alloc`] — the `drop_ctx` a generated context registers.
///
/// # Safety
///
/// `ptr` must be a live [`ctx_alloc`] value of exactly `T`, and this must be the last
/// reference to it.
#[doc(hidden)]
pub unsafe fn drop_box<T>(ptr: *mut c_void) {
    // SAFETY: the caller's contract is exactly `Box::from_raw`'s.
    drop(unsafe { Box::from_raw(ptr.cast::<T>()) });
}

/// Take one more reference to the context at `ptr` (KHook's, released by the removal function
/// it will call).
///
/// # Safety
///
/// `ptr` must be a live context from [`ctx_alloc`] with at least one reference outstanding.
#[doc(hidden)]
pub unsafe fn retain(ptr: *mut c_void) {
    // SAFETY: the caller guarantees the context is live; the header is its first field.
    let header = ptr.cast::<CtxHeader>();
    // SAFETY: as above; the count is atomic, so no data race with a concurrent release.
    unsafe { (*header).refs.fetch_add(1, Ordering::Relaxed) };
}

/// Release one reference to the context at `ptr`, freeing it when it was the last.
///
/// # Safety
///
/// `ptr` must be a live context from [`ctx_alloc`] with at least one reference outstanding.
#[doc(hidden)]
pub unsafe fn release(ptr: *mut c_void) {
    // SAFETY: the caller guarantees the context is live; the header is its first field.
    let header = ptr.cast::<CtxHeader>();
    // SAFETY: as above; `AcqRel` publishes everything earlier writers did with the context
    // before the freeing thread's `drop_ctx`.
    let last = unsafe { (*header).refs.fetch_sub(1, Ordering::AcqRel) } == 1;
    if last {
        // SAFETY: the count just went to zero, so no other reference exists; `drop_ctx` is the
        // concrete dropper registered by the context's own `CtxHeader::new`.
        unsafe { ((*header).drop_ctx)(ptr) };
    }
}

/// The removal path: KHook calls the removal function once per hook id, so this guards against
/// a second call turning a leak into a use-after-free, and it is where the process-global
/// [`Registry::live`] count is lowered and the id leaves [`Registry::pending`] and
/// [`Registry::tracked`].
///
/// **Nothing here touches the context until [`admit_removal`] has answered, by id alone.**
/// [`reconcile_lost`] decided KHook had given an id up and leaked its context on purpose; a
/// stale re-queued insert entry can still hand that removal function back to us for up to
/// (entries ahead of it + 1) insert-thread passes (`detour.cpp:1618-1622` then `:1819-1850`), and
/// the removal function of a `Drop` whose delete-thread pass ran can be handed back the same way.
/// Returning on the id alone — no `removed_fired` swap, no context release, no count — is what
/// makes that call harmless even though the context may already be gone (the [`Hook`]'s own
/// reference, or a user's `remove_sync` that chose not to free it).
///
/// **The lost check, the `removal_fired` check and the claim on [`Registry::tracked`] are one
/// critical section.** An id is accounted for by exactly one of the two removal-path accounts
/// (this claim, or [`reconcile_lost`]'s): a reconcile slipping in between the check and the claim
/// would lower [`Registry::live`] for a context this function is about to release (or the other
/// way round), and a second removal call could release a context the first one already freed.
///
/// This runs on whichever thread KHook removes the hook from — `RemoveHook`'s delete thread for
/// the asynchronous removal [`Drop`] asks for (`detour.cpp:1630-1669`), the removing thread
/// itself for the synchronous one ([`remove_all_blocking`], [`Hook::remove_sync`]) — and it is
/// the **last** thing of this module KHook runs for that hook.
///
/// It never takes [`KHOOK_GATE`]: KHook calls it from inside a `RemoveHook` this module issued
/// while holding that gate (or from its delete thread while a gate holder waits for that
/// thread's work to retire), so taking it here would deadlock.
///
/// This is the variant for a caller that already has the context pointer. The generated thunks
/// use [`release_on_removal_by_id`], which fetches it — and must, because it may not panic.
///
/// # Safety
///
/// `ptr` must be a live context from [`ctx_alloc`] that KHook holds a reference to, and `id`
/// must be the hook id it was registered under.
#[doc(hidden)]
pub unsafe fn release_on_removal(id: HookId, ptr: *mut c_void) {
    probe_removal(id);
    if admit_removal(id) == Admission::Ignore {
        return;
    }
    // SAFETY: the caller's contract is `complete_release`'s.
    unsafe { complete_release(id, ptr) };
}

/// [`release_on_removal`] for the generated `removed` thunk, which KHook calls with the id only.
///
/// Nothing reachable from here may panic: this is `extern "C"` code called by KHook, so a panic
/// is a non-unwinding abort that kills the game. Every step that can fail is answered with a
/// diagnostic line and a leak:
///
/// * no API installed → the context cannot be fetched, so KHook's reference is **leaked** and the
///   id leaves the registry anyway (KHook has run the last thing of this module it will run for
///   it, and [`Registry::removal_fired`] keeps every later call away from the context);
/// * `GetContextPtr` answers null (a broken KHook, or a call outside a thunk) → the same answer.
///
/// # Safety
///
/// `id` must be the hook id KHook is calling the removal function for. Only valid inside that
/// call: `GetContextPtr` answers the context KHook pushed around it (`detour.cpp:1536-1541`).
#[doc(hidden)]
pub unsafe fn release_on_removal_by_id(id: HookId) {
    probe_removal(id);
    if admit_removal(id) == Admission::Ignore {
        return;
    }
    let Some(api) = api() else {
        // KHook's reference stays: nothing of ours can reach that context any more, and
        // `admit_removal` has already recorded that this id's removal function ran. Leaking the
        // allocation is the only safe answer — freeing it would need the pointer.
        if account_removal(id) {
            emit_diagnostic(&underflow_line());
        }
        emit_diagnostic(&missing_api_removal_line(id));
        REGISTRY_CHANGED.notify_all();
        return;
    };
    let ptr = api.get_context_ptr();
    if ptr.is_null() {
        // `GetContextPtr` reads the thread's current hook (`detour.cpp:359-361`); KHook pushes it
        // around the removal function (`:1536-1541`, `:1838-1843`, `:1434-1436`), so a null is a
        // broken KHook. Same answer as the missing API: leak, account, report, never panic.
        if account_removal(id) {
            emit_diagnostic(&underflow_line());
        }
        emit_diagnostic(&unreachable_context_line(id));
        REGISTRY_CHANGED.notify_all();
        return;
    }
    // SAFETY: KHook pushed the registering context for this id around this call, which is
    // `complete_release`'s contract.
    unsafe { complete_release(id, ptr) };
}

/// Where an id stood when its removal function entered, decided **by id alone**
/// ([`admit_removal`]): no API call and no dereference of a context pointer happen before this
/// is known, because a reconciled or already-removed id's context may already be freed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// Reconciled lost ([`Registry::lost`]) or the removal function already ran for it
    /// ([`Registry::removal_fired`]): the call must touch nothing at all.
    Ignore,
    /// The id was in [`Registry::tracked`] and this call took it out: KHook's reference is this
    /// call's to release.
    Claimed,
    /// No set held the id: KHook ran the removal function right after `setup_hook` returned and
    /// before [`finish_registration`] could record it (see
    /// [`Registry::removed_before_registration`]). The id is noted there so the registration does
    /// not record it too.
    Unclaimed,
}

/// Decide what a removal function may do, by id alone, and claim the id for this call.
///
/// This is the **only** thing a removal function may do before it knows the id is still its own:
/// no API call, no `GetContextPtr`, no dereference. See [`Admission`].
fn admit_removal(id: HookId) -> Admission {
    let mut registry = registry();
    // First, before any context is looked at: a reconciled id's context may already have been
    // freed, and a second call for an id whose removal function already ran must not free it
    // again.
    if registry.lost.contains(&id) || registry.removal_fired.contains(&id) {
        return Admission::Ignore;
    }
    // Recorded in the same critical section as the claim, so a later call for the same id sees
    // it whatever happens to the context below.
    registry.removal_fired.insert(id);
    // The claim: exactly one of this function and [`reconcile_lost`] accounts for an id.
    if registry.tracked.remove(&id) {
        return Admission::Claimed;
    }
    // KHook ran the removal function for an id that was never recorded as tracked: the
    // registration is still between `setup_hook` returning and [`finish_registration`] taking
    // the registry (see [`Registry::removed_before_registration`]). Note it so that function
    // does not register an id KHook has already finished with.
    registry.removed_before_registration.insert(id);
    Admission::Unclaimed
}

/// Take an id out of the registry after its removal function ran, lowering [`Registry::live`].
///
/// The half [`complete_release`] and the no-API paths of [`release_on_removal_by_id`] share.
/// Returns whether the count underflowed, which means an id was accounted for twice; the caller
/// reports it after [`REGISTRY`] is released (a count that is already zero must not wrap, and
/// release builds must not panic on KHook's thread).
fn account_removal(id: HookId) -> bool {
    let mut underflow = false;
    {
        let mut registry = registry();
        registry.installed.remove(&id);
        registry.pending.remove(&id);
        // `remove_all_blocking` returns on the strength of the count reaching zero and its caller
        // is allowed to let this module go, so nothing of ours may follow this but the removal
        // function's own return. A count that is already zero means an id was accounted for
        // twice; debug builds make that visible here, release builds leave the count alone and
        // report it.
        match registry.live.checked_sub(1) {
            Some(live) => registry.live = live,
            None => {
                debug_assert!(
                    false,
                    "hook context count underflowed: an id was accounted for twice"
                );
                underflow = true;
            }
        }
        // What follows this function's own return is still this module running on KHook's
        // thread: the condvar signal, the `removed` thunk's epilogue, and KHook's own bookkeeping
        // releasing the mutexes it held around the call. [`remove_all_blocking`]'s barrier waits
        // for that before it reports `Ok`.
        registry.unbarriered_removal = true;
    }
    underflow
}

/// The tail of the removal path once the context is known: release KHook's reference, take the id
/// out of the registry and lower the count.
///
/// # Safety
///
/// `ptr` must be a live context from [`ctx_alloc`] that KHook holds a reference to, and this must
/// be the first removal-function call for `id` ([`admit_removal`] guaranteed that).
unsafe fn complete_release(id: HookId, ptr: *mut c_void) {
    // SAFETY: the caller guarantees the context is live; the header is its first field.
    let header = ptr.cast::<CtxHeader>();
    // SAFETY: as above; the flag is atomic.
    let already = unsafe { (*header).removed_fired.swap(true, Ordering::AcqRel) };
    if already {
        // `admit_removal` already keeps a second call for the same id out, so this needs a caller
        // that bypassed it (`release_on_removal` with an id another path had claimed) or a KHook
        // that called one context's removal function twice. Leaking beats freeing a context a
        // second removal call might still use; the assert makes the contract violation visible in
        // debug builds (and, because this runs inside an `extern "C"` thunk, aborts there).
        debug_assert!(
            false,
            "KHook called the removal function twice for one hook id"
        );
        return;
    }
    // KHook's reference, released exactly once. This is the last use of the context: it may
    // free it here, which runs the destructors of the user callbacks it owns, so it comes
    // *before* the count is lowered — the count must not reach zero while this module still has
    // work to do for this hook.
    // SAFETY: this is KHook's reference, released exactly once.
    unsafe { release(ptr) };
    // Now KHook is done with this context: it leaves the registry — the set of hooks still
    // registered, the one of removals asked for but not yet run — and the process-global count
    // is lowered. `tracked` was already claimed by `admit_removal`.
    if account_removal(id) {
        // Not under `REGISTRY`: the sink is user code and may log through anything.
        emit_diagnostic(&underflow_line());
    }
    REGISTRY_CHANGED.notify_all();
}

/// The context KHook says is running on this thread (`GetContextPtr`, `khook.hpp:2189`), as the
/// generated context type, or `None` when no API is installed or KHook answers null — the answer
/// the thunks take so that a missing API (or a broken `IKHook`) is a diagnostic line rather than
/// a panic or a null dereference in `extern "C"`.
///
/// # Safety
///
/// Only valid inside a thunk of the hook whose context was `T`.
#[doc(hidden)]
#[must_use]
pub unsafe fn ctx_opt<T>() -> Option<*mut T> {
    let ptr = api()?.get_context_ptr().cast::<T>();
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

/// The context KHook says is running on this thread, panicking when no API is installed.
///
/// Not used by the generated thunks any more — they use [`ctx_opt`], because a panic in an
/// `extern "C"` thunk aborts the process. Kept for callers that want the loud version.
///
/// # Safety
///
/// Only valid inside a thunk of the hook whose context was `T`.
#[doc(hidden)]
#[must_use]
pub unsafe fn ctx_raw<T>() -> *mut T {
    api_expect().get_context_ptr().cast::<T>()
}

/// Runs one dispatch: clones the hook's callbacks under its lock and hands the copy to
/// `body`.
///
/// The clone is the same move the C++ templates make (`khook.hpp:733-739`: "This can deadlock
/// (in case of recalls) so make a deep-copy"), for the same two reasons: the storage must not
/// be borrowed while a callback runs (a callback may re-enter the hooked function), and KHook
/// calls a hook from whatever thread reached the hooked function (the trampoline copies the
/// machine state, not a thread).
///
/// # Safety
///
/// `slot` must point at a live, initialised `Mutex` inside a hook context that outlives the
/// call; the only way the vector behind it is reached is through that mutex.
#[doc(hidden)]
pub unsafe fn dispatch<C, R>(
    slot: *const std::sync::Mutex<Vec<Arc<C>>>,
    body: impl FnOnce(&[Arc<C>]) -> R,
) -> R
where
    C: ?Sized,
{
    // SAFETY: the caller guarantees the mutex is live; a shared reference is what a mutex
    // wants, and every writer takes the lock rather than making a `&mut`.
    let slot = unsafe { &*slot };
    let snapshot: Vec<Arc<C>> = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    body(&snapshot)
}

// ---------------------------------------------------------------------------------------
// Return-value helpers (the `init_op`/`deinit_op`/`SaveReturnValue` trio)
// ---------------------------------------------------------------------------------------

/// `khook.hpp:307-310` `init_operator<TYPE>`: `new (assignee) TYPE(*value)` — a copy.
///
/// # Safety
///
/// `dst` must point at uninitialised storage for `R` and `src` at an initialised `R`.
#[doc(hidden)]
pub unsafe extern "C" fn init_copy<R: Copy>(dst: *mut c_void, src: *const c_void) {
    // SAFETY: the caller's contract is `ptr::write`'s plus the source being readable.
    unsafe { ptr::write(dst.cast::<R>(), *src.cast::<R>()) };
}

/// `khook.hpp:312-315` `deinit_operator<TYPE>` runs the destructor; a `Copy` Rust type has
/// none, so this is a no-op (and `khook.hpp:339-344` only ever hands it an initialised value).
///
/// # Safety
///
/// The pointer is never read; the signature is what KHook calls.
#[doc(hidden)]
pub unsafe extern "C" fn deinit_noop<R: Copy>(_dst: *mut c_void) {}

/// `SaveReturnValue` for a Rust return type: `khook.hpp:333-347`'s
/// `__internal__savereturnvalue`, whose `void` case passes null pointers and size 0 — which is
/// what a zero-sized `R` does here, so a void hook needs no separate helper.
///
/// # Safety
///
/// Only valid inside a thunk; `value` must be readable for `size_of::<R>()` bytes.
#[doc(hidden)]
pub unsafe fn save_value<R: Copy>(action: Action, value: *const R, original: bool) {
    let (ptr_to_return, return_size, init_op, deinit_op) = if size_of::<R>() == 0 {
        // `khook.hpp:335-338`: nothing to copy, KHook ignores the pointers.
        (ptr::null_mut(), 0usize, ptr::null_mut(), ptr::null_mut())
    } else {
        (
            value.cast_mut().cast::<c_void>(),
            size_of::<R>(),
            init_copy::<R> as *mut c_void,
            deinit_noop::<R> as *mut c_void,
        )
    };
    // A thunk of this module runs only while an API is installed (KHook's registration stored
    // that thunk), but the invariant can be broken by `uninstall` and a panic here would abort
    // the game: report and keep the call going with no saved value instead.
    let Some(api) = api() else {
        report_thunk_without_api("save_value");
        return;
    };
    // SAFETY: the caller was entered as a thunk of this hook and the pointers follow
    // `SaveReturnValue`'s contract (`khook.hpp:293`, impl `detour.cpp:366-406`).
    unsafe {
        api.save_return_value(action, ptr_to_return, return_size, init_op, deinit_op, original)
    };
}

// ---------------------------------------------------------------------------------------
// Stack sizes (khook.hpp:83-104)
// ---------------------------------------------------------------------------------------

/// `khook.hpp:83-95` — `_copy_stack_size()` on `_WIN64`.
///
/// `param_count` is the number of machine arguments the thunk takes, `this` included (the C++
/// always prepends one `void*` to the parameter pack for this, `khook.hpp:684`). A non-void
/// return adds one more slot "even if the return value doesn't end up being pushed as an arg,
/// better to copy more than not enough"; the first four go in registers and the +32 is the
/// shadow space KHook's own calls need.
#[must_use]
pub const fn stack_size_win64(param_count: usize, non_void_return: bool) -> u32 {
    let num_args = param_count + if non_void_return { 1 } else { 0 };
    // The header's `(num_args <= 4) ? 0 : (num_args - 4)`.
    let num_stack_args = num_args.saturating_sub(4);
    (num_stack_args * 8 + 32) as u32
}

/// `khook.hpp:96-103` — `_copy_stack_size()` off `_WIN64`: every argument takes at least a
/// pointer slot, plus one for a non-void return.
#[must_use]
pub const fn stack_size_linux(arg_sizes: &[usize], return_size: Option<usize>) -> u32 {
    let mut total = match return_size {
        Some(size) => {
            if size > 8 {
                size
            } else {
                8
            }
        }
        None => 0,
    };
    let mut i = 0;
    while i < arg_sizes.len() {
        let size = arg_sizes[i];
        total += if size > 8 { size } else { 8 };
        i += 1;
    }
    total as u32
}

/// `sizeof(ARGS)` (`khook.hpp:101-102`) for a parameter declared as a C++ **reference**.
///
/// C++'s `sizeof` on a reference type is the size of the referenced type ([expr.sizeof]/2), and
/// KHook's `(std::max)(sizeof(void*), sizeof(ARGS)) + ...` over `void*, ARGS...` therefore adds
/// `sizeof(QAngle)` = 12 for a `const QAngle&` parameter, not a pointer slot. Under-copying the
/// stack is the direction that corrupts the caller, so a reference parameter must be marked in
/// the macro ([`khook_thunks!`]) and this trait is what turns the Rust mirror's pointer type into
/// the number KHook uses.
///
/// Only the pointer-shaped Rust mirrors below implement it, which is what makes the marker safe:
/// the size passed to [`stack_size_linux`] is `size_of` of the *pointee*, exactly the C++
/// referent. A `ref` on anything else (a `&T`, say) is a missing-impl compile error rather than
/// a wrong number. An rvalue reference is covered by the same two impls: `sizeof(T&&)` is
/// `sizeof(T)` as well, and its Rust mirror is a pointer too.
pub trait CppReferent {
    /// `sizeof(T&)` for the `T` behind this Rust mirror.
    const SIZE: usize;
}

impl<T> CppReferent for *const T {
    /// `khook.hpp:101-102`: `sizeof(ARGS)` for `const T&` is `sizeof(T)`.
    const SIZE: usize = size_of::<T>();
}

impl<T> CppReferent for *mut T {
    /// `khook.hpp:101-102`: `sizeof(ARGS)` for `T&` is `sizeof(T)`.
    const SIZE: usize = size_of::<T>();
}

/// One parameter's contribution to the Linux stack-size sum (internal): `sizeof(ARG)` with
/// `val` (the C++ parameter is by value or a pointer, so the Rust type's size *is* `sizeof(ARG)`)
/// and the referent's size with `ref` ([`CppReferent`]).
///
/// An unknown marker is a compile error naming it rather than a silently too-small stack size.
#[doc(hidden)]
#[macro_export]
macro_rules! __khook_arg_size {
    (val, $aty:ty) => {
        core::mem::size_of::<$aty>()
    };
    (ref, $aty:ty) => {
        <$aty as $crate::hook::CppReferent>::SIZE
    };
    ($other:tt, $aty:ty) => {
        compile_error!(concat!(
            "unknown KHook parameter marker `",
            stringify!($other),
            "`: expected `val` (by value or pointer) or `ref` (C++ reference)"
        ))
    };
}

/// The compile-time half of the declared-vs-computed check: a caller that carries a stack size
/// from generated IR can write
/// `const _: u32 = s2sdk::hook::assert_stack_size(DECLARED, generated_mod::STACK_SIZE);`
/// and a mismatch becomes a compile error naming both numbers. The runtime half is the
/// `declared_stack_size` argument of a generated `setup_inline`/`setup_virtual`
/// ([`Error::StackSizeMismatch`]).
///
/// # Panics
///
/// Fails const evaluation when `declared != computed`:
///
/// ```compile_fail
/// // error: evaluation of constant value failed: declared stack size does not match ...
/// const _: u32 = s2sdk::hook::assert_stack_size(16, 24);
/// # fn main() {}
/// ```
#[must_use]
pub const fn assert_stack_size(declared: u32, computed: u32) -> u32 {
    assert!(
        declared == computed,
        "declared stack size does not match the one computed from the signature"
    );
    computed
}

/// `1` per argument: lets a `const` expression count the macro's parameters.
#[doc(hidden)]
#[must_use]
pub const fn one_of<T>() -> usize {
    1
}

/// The declared-vs-computed check a generated `setup_*` runs before registering.
///
/// It is a **cross-check**, not a safety bound: it refuses *any* disagreement, so a declared
/// size that is larger than the computed one is refused too (it names both numbers). A larger
/// declared size would make KHook copy more stack than the signature needs — harmless for the
/// callee, but it means the declared number and the signature describe different calls, and
/// KHook allocates its copy stack from the *first* registration's number anyway
/// ([`register_inline`]).
///
/// # Errors
///
/// [`Error::StackSizeMismatch`] when `declared` is `Some` and differs from `computed`, in
/// either direction.
pub fn check_stack_size(computed: u32, declared: Option<u32>) -> Result<u32> {
    match declared {
        None => Ok(computed),
        Some(declared) if declared == computed => Ok(computed),
        Some(declared) => Err(Error::StackSizeMismatch { declared, computed }),
    }
}

// ---------------------------------------------------------------------------------------
// Registration and the RAII Hook
// ---------------------------------------------------------------------------------------

/// The process-global bookkeeping behind [`live_context_count`], [`remove_all_blocking`] and the
/// per-address `DetourCapsule` stack sizes.
struct Registry {
    /// The ids of hooks registered through this module that nobody has asked KHook to remove
    /// yet — the registered half of what [`remove_all_blocking`] removes synchronously.
    installed: BTreeMap<HookId, ()>,
    /// The ids a [`Hook`] was dropped for, whose removal [`Drop`] asked KHook to do
    /// asynchronously and whose removal function has not run yet — the other half of what
    /// [`remove_all_blocking`] re-issues synchronously. An id leaves this set in
    /// [`release_on_removal`], so it holds exactly the removals KHook still has to catch up
    /// with, whether they sit in its delete queue or in its not-yet-inserted queue. It also
    /// holds a [`Drop`]'s id whose removal was never issued because no API was installed then
    /// ([`Drop`] returns from that path without asking KHook), so a later [`remove_all_blocking`]
    /// re-issues it.
    pending: BTreeMap<HookId, ()>,
    /// Every id KHook accepted for which this module has not yet seen the removal function run:
    /// [`finish_registration`] adds, the removal path's [`admit_removal`] claims.
    ///
    /// It is what makes a lost removal *detectable*: after a **synchronous** removal returned, an
    /// id still here is one KHook no longer holds and will never call the removal function of
    /// ([`reconcile_lost`]).
    tracked: BTreeSet<HookId>,
    /// Every id whose removal function KHook has entered once, recorded by [`admit_removal`] in
    /// the same critical section as its claim on [`tracked`](Registry::tracked).
    ///
    /// It is what keeps a second call for the same id from touching the context: KHook's
    /// `insert_hooks` scan can hand the same removal function back a second time
    /// (`detour.cpp:1618-1622` then `:1819-1850`), and by then the context may already be freed
    /// (the removal ran, then the [`Hook`]'s own reference went). One entry per hook that was ever
    /// removed — a plugin has a few dozen — and, like [`lost`](Registry::lost), it never shrinks:
    /// KHook's ids are never reused.
    removal_fired: BTreeSet<HookId>,
    /// Ids a synchronous removal found still [`tracked`](Registry::tracked) — KHook lost them to
    /// the insert-thread race (`detour.cpp:1603-1628` against `:1819-1850,1581-1589`).
    ///
    /// This set **never shrinks**, not even across [`install`]/[`uninstall`]: KHook's hook-id
    /// counter is one global of the library (`detour.cpp:1546-1548`, incremented under
    /// `hook_id_mutex` at `:1728-1729`) and Metamod's per-plugin `KHookImpl` forwards to that
    /// same library (`core/metamod_khook.h:49,77`), so an id is never reused in the process. An
    /// id that was lost once must be ignored by [`release_on_removal`] for the rest of the
    /// process: a stale re-queued insert entry can still carry its removal function, and that
    /// call must not touch a context this module has already given up on.
    lost: BTreeSet<HookId>,
    /// The lost ids the drain of [`remove_all_blocking`] has already re-issued once, so its
    /// `drained N lost id(s)` line reports what that call newly drained rather than the whole
    /// permanent set. Every lost id is still re-issued on every drain (a stale entry may be
    /// re-queued at any time); only the *report* is once per id.
    drained: BTreeSet<HookId>,
    /// Ids whose removal function KHook ran before [`finish_registration`] could record them.
    ///
    /// The registration calls `setup_hook` and only afterwards takes [`REGISTRY`] to record the
    /// id, and KHook can run the removal function in that window: `__Setup__Hook` releases
    /// `hooks_detour_mutex` before it returns (`detour.cpp:1750`), so a concurrent `Shutdown`
    /// (`:1885`, exclusive) can clear `hooks_detour` and destroy the capsule
    /// (`:1429-1437`), and so can another plugin's `__Setup__Hook` whose `insert_or_assign`
    /// replaces the capsule this hook was just associated with (`:1701`).
    ///
    /// The removal function therefore cannot run *inside* `SetupHook`: `__Setup__Hook` holds
    /// `hooks_detour_mutex` **shared** from `:1722` to `:1750`, and `Shutdown` needs it
    /// exclusive (`:1885`). That is also why this is not a `SetupMode::Sync` case only: the
    /// first registration on an address is forced synchronous (`async = false`, `:1712-1717`),
    /// but an asynchronous registration returns from `setup_hook` into the same window.
    ///
    /// The registration takes KHook's reference and the [`live`](Registry::live) count *before*
    /// it hands the context over for exactly that case; the removal path notes the id here so
    /// `finish_registration` does not register it a second time (and returns a [`Hook`] with no
    /// `installed`/`tracked` entry, which [`Drop`] then skips `RemoveHook` for). Exactly one of
    /// the two may account for an id: this set, or [`tracked`](Registry::tracked).
    removed_before_registration: BTreeSet<HookId>,
    /// How many ids have been reconciled as lost (the sum of every [`reconcile_lost`] that
    /// fired); [`lost_removal_count`].
    lost_count: usize,
    /// The copy-stack size KHook has fixed for each hooked address, rounded up to 16 the way
    /// `DetourCapsule`'s constructor does (`detour.cpp:535-536`), taken from the *first*
    /// registration on that address (`detour.cpp:1696-1719`). KHook keeps a capsule for the life
    /// of the process — `hooks_detour` is only erased when the first setup on an address failed
    /// (`detour.cpp:1706`) or by `Shutdown` (`:1885-1890`) — so this map only shrinks for
    /// the first case, and a later registration needing more than an entry holds is refused
    /// ([`Error::CapsuleStackTooSmall`]).
    capsules: BTreeMap<usize, u32>,
    /// How many hook contexts KHook can still call into: one per successful registration, one
    /// less every time the removal function KHook stored at setup has run. That function is the
    /// last thing of this module KHook ever runs ([`release_on_removal`]), so `0` is the signal
    /// a plugin may unload.
    live: usize,
    /// Set by [`release_on_removal`] when it lowers [`Registry::live`], and taken by
    /// [`remove_all_blocking`]'s barrier: from that decrement until the removal function has
    /// returned into KHook, the rest of *this* module is still on KHook's thread, so a count of
    /// zero alone does not yet mean every removal function has retired.
    unbarriered_removal: bool,
}

impl Registry {
    const fn new() -> Self {
        Self {
            installed: BTreeMap::new(),
            pending: BTreeMap::new(),
            tracked: BTreeSet::new(),
            removal_fired: BTreeSet::new(),
            lost: BTreeSet::new(),
            drained: BTreeSet::new(),
            lost_count: 0,
            removed_before_registration: BTreeSet::new(),
            capsules: BTreeMap::new(),
            live: 0,
            unbarriered_removal: false,
        }
    }
}

/// The copy-stack size KHook rounds a requested stack size up to: `DetourCapsule`'s
/// `_stack_size(((stack_size + 0xF) & ~0xF))` (`detour.cpp:535-536`).
#[must_use]
const fn capsule_stack_size(stack_size: u32) -> u32 {
    (stack_size + 0xF) & !0xF
}

/// Claim the `DetourCapsule` of `address` for a registration whose signature computes
/// `stack_size` bytes of hook stack, and say whether this call created the address's entry.
///
/// KHook creates the capsule on the first registration on an address, from that registration's
/// stack size, and hands every later registration the same capsule with its own `stack_size`
/// argument ignored (`detour.cpp:1696-1719` finds the address in `hooks_detour` and skips the
/// insert). A later signature that needs *more* than the capsule holds would therefore have its
/// copy stack cut short (`detour.cpp:794` copies into it), so it is refused; needing less (or as
/// much, after the round to 16) is fine, and KHook's own behaviour for that case.
///
/// # Errors
///
/// [`Error::CapsuleStackTooSmall`] when the address already has a capsule created for fewer
/// bytes.
fn claim_capsule(address: usize, stack_size: u32) -> Result<bool> {
    let needed = capsule_stack_size(stack_size);
    let mut registry = registry();
    match registry.capsules.get(&address) {
        None => {
            registry.capsules.insert(address, needed);
            Ok(true)
        }
        Some(&capsule) if capsule >= needed => Ok(false),
        Some(&capsule) => Err(Error::CapsuleStackTooSmall {
            address,
            capsule,
            needed,
        }),
    }
}

/// Forget the capsule claim for `address` after KHook refused the registration that was meant to
/// create it: `__Setup__Hook` erases the capsule it had just inserted when its setup fails
/// (`detour.cpp:1706`), so the next registration on that address starts from its own stack
/// size again.
fn release_capsule(address: usize) {
    registry().capsules.remove(&address);
}

/// `live_context_count` / `remove_all_blocking` state. A plain mutex rather than an atomic
/// because the fields have to move together: `remove_all_blocking` takes the ids it is going to
/// remove and the count it is going to wait for in one step, and a removal function lowers the
/// count and removes its id from the same two sets.
static REGISTRY: std::sync::Mutex<Registry> = std::sync::Mutex::new(Registry::new());

/// Signalled by `release_on_removal` once its work is done; [`remove_all_blocking`] waits here.
static REGISTRY_CHANGED: Condvar = Condvar::new();

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------
// Lost removals: detection, reconcile, and the diagnostic sink
// ---------------------------------------------------------------------------------------

/// Where the reconcile line of [`reconcile_lost`] and the drain line of
/// [`remove_all_blocking`] go. Null until [`set_diagnostic_sink`] is called; the bridge points
/// it at its log from the plugin's `Load`, and the default is no output.
static DIAGNOSTIC_SINK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Point this module's diagnostics at `f`, typically the bridge's log function.
///
/// It can be called at any time from any thread (a plugin's `Load` is the intended place); the
/// lines that use it are the detected lost removal ([`reconcile_lost`]), the drain that chased
/// one and a count underflow ([`remove_all_blocking`]). Passing a function is the whole
/// interface: a plain `fn(&str)` keeps the sink lock-free and callable from a removal function
/// KHook runs while it holds its own bookkeeping locks, where taking a lock of ours could
/// deadlock with a caller that already holds one.
///
/// **A sink must not call this module's hook API.** The lines [`reconcile_lost`] and
/// [`remove_all_blocking`]'s drain produce are emitted while [`KHOOK_GATE`] is held by the caller
/// that produced them ([`Hook::remove_sync`], [`remove_all_blocking`]); the count-underflow line
/// can also come from a removal function on KHook's delete thread, which never takes the gate at
/// all. A sink that registered, dropped or drained a hook would wait for that same gate and
/// deadlock the thread it logs from. Logging through the bridge's own log, formatting, or
/// touching other state is fine: no lock of this module is held when `f` runs ([`REGISTRY`] is
/// released first, [`KHOOK_GATE`] is not needed for that).
///
/// [`set_diagnostic_sink`] is process-wide and not reset by [`uninstall`]: whichever thread
/// logs, the sink must outlive the plugin that set it, exactly like the API slot.
pub fn set_diagnostic_sink(f: fn(&str)) {
    // A `fn` pointer is a thin pointer to code, so it round-trips through `*mut ()` exactly.
    DIAGNOSTIC_SINK.store(f as *const () as *mut (), Ordering::Release);
}

/// Hand `line` to the installed sink, if any. Never called while [`REGISTRY`] is held, so a
/// sink that formats through this module (or logs through something that does) cannot deadlock.
fn emit_diagnostic(line: &str) {
    let stored = DIAGNOSTIC_SINK.load(Ordering::Acquire);
    if stored.is_null() {
        return;
    }
    // SAFETY: the only writer stores a `fn(&str)` cast to `*mut ()` (see
    // `set_diagnostic_sink`), and the pointer is never cleared, so the value read here is either
    // null (rejected above) or that same function pointer. A plain `fn` pointer is a thin
    // pointer to code (no environment, unlike a closure `dyn Fn`), so the round trip through
    // `*mut ()` is lossless and this `transmute` is the exact inverse of the store-side cast.
    let f: fn(&str) = unsafe { core::mem::transmute::<*mut (), fn(&str)>(stored) };
    f(line);
}

/// Where the *count* of calls into this module's removal path goes, for tests that have to know
/// how often [`release_on_removal`] was entered per id without asking the mock (which only knows
/// what it called). Null = nobody is counting.
static REMOVAL_PROBE: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Count calls into the removal path through `f` (a test hook; `#[doc(hidden)]` because it is
/// not part of the wrapper's contract).
///
/// `f` is called at the very top of [`release_on_removal`] and [`release_on_removal_by_id`], on
/// whichever thread KHook runs the removal function, before anything is read or locked — so it
/// must not call this module's hook API either.
#[doc(hidden)]
pub fn __set_removal_probe(f: fn(HookId)) {
    // A `fn` pointer is a thin pointer to code, so it round-trips through `*mut ()` exactly.
    REMOVAL_PROBE.store(f as *const () as *mut (), Ordering::Release);
}

/// Report one entry into [`release_on_removal`] to the test probe, if one is installed.
fn probe_removal(id: HookId) {
    let stored = REMOVAL_PROBE.load(Ordering::Acquire);
    if stored.is_null() {
        return;
    }
    // SAFETY: the only writer stores a `fn(HookId)` cast to `*mut ()` (see
    // `__set_removal_probe`), and the pointer is never cleared, so the value read here is either
    // null (rejected above) or that same function pointer; a plain `fn` is a thin pointer to
    // code and the round trip is lossless.
    let f: fn(HookId) = unsafe { core::mem::transmute::<*mut (), fn(HookId)>(stored) };
    f(id);
}

/// How many removals KHook lost to its insert-thread race and this module reconciled
/// ([`reconcile_lost`]). Monotonic for the life of the process.
#[must_use]
pub fn lost_removal_count() -> usize {
    registry().lost_count
}

/// What [`reconcile_lost`] did, so its caller can report it after dropping [`REGISTRY`] (a sink
/// must not run under the lock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reconciled {
    /// The id was not tracked: nothing was reconciled, KHook removed it normally.
    Nothing,
    /// The id was reconciled: its context is leaked and [`Registry::live`] came down.
    Lost,
    /// The id was reconciled, but [`Registry::live`] was already zero — an id accounted for
    /// twice, which is a bug on this module's side. The count was left at zero (release builds
    /// must not panic) and the caller reports the line from [`underflow_line`].
    LostWithCountUnderflow,
}

/// The G2 reconcile: call this **with [`REGISTRY`] held**, immediately after a
/// **synchronous** `RemoveHook(id, async = false, ..)` returned.
///
/// If the id is still in [`Registry::tracked`], KHook lost it: it took the hook off its insert
/// queue but a removal found the id associated, found no callback in the capsule to run, and
/// answered "gone" *without* calling our removal function (`detour.cpp:1603-1628` against
/// `:1819-1850,1497-1544,1587-1589`), while the insert thread then dropped the details because
/// the id was no longer associated (`:1563-1571`). Nothing is patched and nothing dangles, but
/// the context, its reference and [`Registry::live`] would stay for ever and
/// [`remove_all_blocking`] would wait out its deadline.
///
/// **Why "still tracked" is a sound detection.** A synchronous `RemoveHook(id)` returns only
/// after every removal-function call for `id` that was already possible has retired: the
/// `insert_hooks` scan runs under `insert_hooks_mutex` *inside* our own call
/// (`detour.cpp:1819-1850`), and the capsule removal runs under `associated_hooks_mutex`
/// **exclusive** (`__RemoveHook_Sync`, `:1581-1589`), which our call also takes. So an id that
/// is still tracked when the call returns is one KHook no longer holds and will not call the
/// removal function for. The one exception is a stale re-queued insert entry (the insert
/// thread's capsule `try_lock` failed, `__InsertHook_Sync` returned false, and our removal
/// erased the association before the re-queue, `:1618-1622`): it can still carry our removal
/// function and context for up to (entries ahead of it + 1) insert-thread passes. That is exactly
/// why the context is **leaked** here — never released, neither KHook's reference nor the
/// [`Hook`]'s — and why the removal path ([`admit_removal`], which both [`release_on_removal`]
/// entry points run first) checks [`Registry::lost`] by id before touching anything.
///
/// The `tracked.remove` here is the claim that makes this reconcile and
/// [`admit_removal`] account for an id exactly once: whoever removes the id from
/// [`Registry::tracked`] first is the one that lowers the count for it.
fn reconcile_lost(registry: &mut Registry, id: HookId) -> Reconciled {
    if !registry.tracked.remove(&id) {
        return Reconciled::Nothing;
    }
    registry.installed.remove(&id);
    registry.pending.remove(&id);
    // The removal function will never run for this id, so the context it guarded is no longer
    // something KHook can call into: the count has to come down here instead. A count that is
    // already zero means the id was accounted for twice; debug builds make that visible,
    // release builds report it instead of wrapping.
    let mut outcome = Reconciled::Lost;
    match registry.live.checked_sub(1) {
        Some(live) => registry.live = live,
        None => {
            debug_assert!(
                false,
                "hook context count underflowed while reconciling a lost removal"
            );
            outcome = Reconciled::LostWithCountUnderflow;
        }
    }
    registry.lost.insert(id);
    registry.lost_count += 1;
    outcome
}

/// The diagnostic [`reconcile_lost`] emits, in one place so the test can assert the exact line.
#[must_use]
fn lost_line(id: HookId) -> String {
    format!("khook: lost removal for id {id} reconciled (KHook insert race)")
}

/// The diagnostic for a [`Registry::live`] decrement that found zero: an id was accounted for
/// twice. Never expected; reported rather than panicked on in release builds.
#[must_use]
fn underflow_line() -> String {
    "khook: hook context count underflowed (an id was accounted for twice)".to_owned()
}

/// The diagnostic for a removal function KHook ran with no API installed: its context cannot be
/// reached any more, so KHook's reference (and the allocation) is leaked on purpose.
#[must_use]
fn missing_api_removal_line(id: HookId) -> String {
    format!(
        "khook: removal for hook id {id} ran with no KHook IKHook installed; its context is \
         leaked"
    )
}

/// The diagnostic for a removal function whose `GetContextPtr` answered null: a broken `IKHook`.
/// The id is accounted for and its context leaked, exactly like the missing-API case.
#[must_use]
fn unreachable_context_line(id: HookId) -> String {
    format!(
        "khook: removal for hook id {id} could not reach its context (GetContextPtr answered \
         null); its context is leaked"
    )
}

/// The diagnostic for a [`Hook`] dropped with no API installed: the removal cannot be asked for,
/// so KHook keeps the registration and its own reference (the [`Hook`] releases only its own).
#[must_use]
fn missing_api_drop_line(id: HookId) -> String {
    format!(
        "khook: hook id {id} dropped with no KHook IKHook installed; no removal can be asked \
         for (the registration is leaked)"
    )
}

/// Report a [`reconcile_lost`] outcome after [`REGISTRY`] has been released, and say whether the
/// id was reconciled at all.
fn report_reconcile(id: HookId, outcome: Reconciled) -> bool {
    if outcome == Reconciled::Nothing {
        return false;
    }
    emit_diagnostic(&lost_line(id));
    if outcome == Reconciled::LostWithCountUnderflow {
        emit_diagnostic(&underflow_line());
    }
    REGISTRY_CHANGED.notify_all();
    true
}

/// How many hook contexts KHook can still call into: raised by a successful registration,
/// lowered by the removal function KHook runs for it. `0` means this module's thunks, its
/// removal functions and its contexts are all finished with as far as KHook is concerned.
///
/// A live [`Hook`] whose removal already ran does not count: KHook has forgotten the hook and
/// will not call into it again.
#[must_use]
pub fn live_context_count() -> usize {
    registry().live
}

/// Remove every hook this module still has registered, then wait for KHook to be finished with
/// all of them — the last thing a plugin does before it unloads.
///
/// It (a) asks KHook to remove **synchronously** (`async = false`) every id this module knows
/// about: the ones whose [`Hook`] is still alive and the ones whose removal [`Drop`] already
/// queued asynchronously, and (b) waits on [`live_context_count`] until it reaches zero or
/// `deadline` passes. It does **not** leave the queued removals to KHook's delete thread:
/// KHook's own removal functions are code of this module, and
///
/// * the delete thread may be gone already (`KHook::Shutdown` joins it, `detour.cpp:1879-1895`)
///   while a hook it never got to — one still waiting in `insert_hooks`, whose capsule is
///   destroyed without walking it — keeps its removal function unrun,
/// * and a waiter woken by a removal function on that thread would resume while that function's
///   own epilogue is still on the thread's stack.
///
/// Re-issuing is what KHook itself guarantees to be strong: `__RemoveHook_Sync` holds
/// `associated_hooks_mutex` **exclusively** around the capsule's `RemoveHook`, which is what
/// runs this module's removal function (`detour.cpp:1581-1589,1536-1541`), so a synchronous
/// removal returns only after any concurrent removal function of that id has fully retired, and
/// exactly one of the two can win. An id KHook no longer holds is a no-op ("not associated",
/// `:1582-1585`) and a not-yet-inserted one is removed by the `insert_hooks` scan
/// (`:1819-1850`), which also runs the removal function.
///
/// A synchronous removal can still return with an id KHook *lost* — its insert thread took the
/// id off `insert_hooks` and, before the capsule had it, the removal found the id associated but
/// no callback to remove and erased the association without calling the removal function
/// (`detour.cpp:1603-1628` against `:1819-1850,1497-1544,1587-1589`). After every such call this
/// function reconciles against [`Registry::tracked`] ([`reconcile_lost`]): the id is counted as
/// lost, its context leaked on purpose, and it is no longer waited for. The loop is followed by
/// one more synchronous removal per lost id — the drain described in the body — and by one more
/// synchronous removal of [`INVALID_HOOK`] whenever a removal function of this module has already
/// run (see below), which is what makes the count reaching zero sufficient.
///
/// An id [`Drop`] lost that way keeps [`live_context_count`] above zero until a call of this
/// function re-issues it synchronously and reconciles it: the removal function will never run on
/// its own, and the delete thread may be gone. That is the one case in which a dropped hook is
/// still counted, which is exactly why this function has to be called before an unload.
///
/// # Errors
///
/// [`Error::InThunk`] when called from inside a hook thunk of this module (see below), without
/// touching anything. [`Error::UnloadTimeout`] with the count of contexts that were still live
/// when `deadline` passed. With the lost-removal reconcile in place this is a **defensive
/// backstop**: a lost removal is reconciled instead of waited out, every synchronous removal that
/// returns brought every removal-function call it could have made to an end, and the gate keeps
/// registrations from racing the snapshot, so the only way left for the count to stay above what
/// [`Registry::tracked`] knows is a broken invariant on this module's side. Reporting it is still
/// the only safe answer — unloading with a context KHook may call into is a crash.
/// [`Error::KHookNotInstalled`] means there is something left to remove or wait for and no API to
/// do it with: nothing was removed and nothing was waited for, and the ids are kept, so a later
/// call with an API installed still has them. A registry with nothing left but lost ids (which
/// need nothing from us — KHook no longer holds them) is `Ok` without an API rather than an error
/// for ever. A pending [`Registry::unbarriered_removal`] (a removal function of this module is
/// still on its way back into KHook, or one ran with no API so the barrier below could not be
/// issued) is an error too: the count alone does not prove that nothing of ours is still on
/// KHook's thread.
///
/// # When *not* to call this, and what it needs to be true
///
/// **Never from inside a hook callback, and never from the call stack of a hooked function.**
/// KHook holds the hooked address's mutex shared for the whole of a hooked call and removal
/// needs it exclusively (`detour.cpp:1503,1581-1587`); a synchronous removal of a target on the
/// current stack spins until it gets that mutex, which cannot happen until the call returns —
/// so the loop never returns and `deadline` **cannot bound it**: it bounds only the wait after
/// the loop. Removal from a hooked call stack has to go through [`Drop`] (asynchronous) instead,
/// and this function refuses the thunk case outright with [`Error::InThunk`] in every build.
///
/// **No hook may be registered or dropped while this runs.** The whole function holds
/// [`KHOOK_GATE`], so a concurrent [`register_inline`]/[`register_virtual`]/[`Drop`]/
/// [`Hook::remove_sync`] waits for it to return instead of landing between the snapshot and the
/// removals (which would make the snapshot wait for a hook it never re-issued, or queue a
/// removal the delete thread runs after this returned). A caller still has to drop every
/// [`Hook`] first and stop registering: waiting for the gate is not a substitute for the plugin's
/// own unload order.
///
/// The in-thunk exceptions do not wait for the gate either: a [`Drop`] **from inside a thunk**
/// skips it (it cannot wait for a gate holder that may itself be spinning on this call's
/// capsule, see [`Drop`]), and a registration from inside a thunk `try_lock`s it and fails with
/// [`Error::GateBusy`] instead of waiting. Such a drop can land after this function's snapshot:
/// the id then sits in [`Registry::pending`] with no re-issue from the loop, and only the wait
/// covers it — the delete thread runs its removal function, or the wait times out
/// ([`Error::UnloadTimeout`]). A registration that answered [`Error::GateBusy`] never reached
/// KHook, so it cannot race the snapshot. That is the ordering a dropping or registering
/// callback is stuck with; do not tear hooks down from callbacks while unloading.
///
/// Every live [`Hook`] must be dropped before this module (or the plugin it lives in) unloads.
/// This function makes KHook forget the hooks and waits for KHook's threads to stop calling
/// into us, but a [`Hook`]'s own [`Drop`] is still code of this module: dropping one after the
/// image is gone would call `RemoveHook` through a dangling `IKHook`.
///
/// # What `Ok(())` does and does not promise
///
/// `Ok(())` means every removal function of this module has *returned into KHook*: the ones for
/// the ids in the loop above ran inside it (KHook calls them while holding
/// `associated_hooks_mutex` exclusively, `detour.cpp:1581-1589`), a removal function the delete
/// thread had started earlier had to finish before that same mutex let this loop's call for its
/// id return, and the barrier described above waits for a removal function whose count this
/// function saw reach zero while it was still on KHook's thread. So unlike a design that waits
/// for the count alone, the waiter is not racing a removal function's epilogue on another stack.
///
/// The count is still lowered *inside* that function, after the last use of the hook's context
/// (which may free it, and with it run the destructors of the user callbacks it owns), and those
/// destructors run while KHook holds its per-hook bookkeeping locks — so a destructor must not
/// call back into KHook (no registering, no [`Drop`], no [`remove_all_blocking`], and no calling
/// the hooked function). The promise is "no thunk and no removal function of this module will be
/// entered any more" rather than "this module may already be unmapped": the caller unmaps its
/// own image.
pub fn remove_all_blocking(deadline: Duration) -> Result<()> {
    // A synchronous removal from a thunk spins for ever on the capsule this call holds shared
    // (`detour.cpp:214,1503,1596`): refused right here, before anything is asked of KHook, in
    // every build.
    if in_thunk() {
        return Err(Error::InThunk);
    }
    // G1: registrations and removals of this module are serialised against this function for
    // its whole life, including the wait below (see [`KHOOK_GATE`]). That is what makes the
    // snapshot and the reconcile sound: no [`Hook`] can be dropped and no hook registered
    // between the id snapshot and the last `RemoveHook`, so "still tracked after the call
    // returned" cannot be one of our own registrations that has not been recorded yet.
    let _gate = khook_gate();
    // The API is looked up **before** the registry is touched: taking the ids out of
    // `installed`/`pending` and then answering `KHookNotInstalled` would lose them, and no later
    // call could remove them. Lost ids do not need an API (KHook no longer holds them), so a
    // registry that has nothing else left is `Ok` rather than an error for ever.
    let api = api();
    // One lock scope: the ids to remove, the count to wait for and whether a removal function is
    // already on its way out of this module have to be read at the same moment. Read separately,
    // a registration that completed in between would be counted as something to wait for
    // without its id being in the set that gets removed — and this function would then wait out
    // the whole deadline for a hook it never asked KHook to remove.
    let (installed_ids, pending_ids, live) = {
        let mut registry = registry();
        if api.is_none() {
            if registry.installed.is_empty()
                && registry.pending.is_empty()
                && registry.live == 0
                && !registry.unbarriered_removal
            {
                // Nothing KHook can still call into, nothing to wait for, no removal function on
                // its way back into KHook, nothing to remove and no stale entry we could drain
                // without an API: nothing to do.
                return Ok(());
            }
            // `unbarriered_removal` keeps this an error rather than `Ok`: a removal function of
            // this module is still on its way back into KHook (or one ran with no API and the
            // barrier could not be issued), so "every removal function has returned" is not
            // proven.
            return Err(Error::KHookNotInstalled);
        }
        // Both halves of KHook's remaining work: what is still registered and what `Drop`
        // already queued asynchronously. Emptying them here is what keeps a removal function
        // that runs inside the calls below (they take the registry lock) from finding its own id
        // still in the set.
        let installed = core::mem::take(&mut registry.installed);
        let pending = core::mem::take(&mut registry.pending);
        (
            installed.into_keys().collect::<Vec<HookId>>(),
            pending.into_keys().collect::<Vec<HookId>>(),
            registry.live,
        )
    };
    // No panic even here: the API was `Some` under the registry lock above, but `uninstall` is
    // not serialised by the gate, and a panic in this function (called from a plugin's `Unload`)
    // is still a panic in a plugin callback.
    let Some(api) = api else {
        // Nothing was removed; put the ids back where they came from so a later call with an API
        // installed still has them.
        let mut registry = registry();
        registry
            .installed
            .extend(installed_ids.iter().map(|id| (*id, ())));
        registry
            .pending
            .extend(pending_ids.iter().map(|id| (*id, ())));
        return Err(Error::KHookNotInstalled);
    };
    for id in installed_ids.iter().chain(pending_ids.iter()).copied() {
        // SAFETY: `id` is a live hook id of this module's registry, and the removal
        // function KHook stored at setup is the one it calls, so both caller-supplied slots
        // are null (`khook.hpp:2188`, `detour.cpp:1812-1877`).
        unsafe {
            api.remove_hook(
                id,
                false,
                ptr::null_mut::<c_void>(),
                ptr::null_mut::<c_void>(),
            )
        };
        // G2: the call above is synchronous, so if the id is still tracked KHook lost it (see
        // [`reconcile_lost`]) — it is no longer something to wait for, and waiting for it is
        // exactly the `UnloadTimeout` this reconcile replaces.
        let outcome = {
            let mut registry = registry();
            reconcile_lost(&mut registry, id)
        };
        // Not under `REGISTRY`: the sink is user code and may log through anything.
        report_reconcile(id, outcome);
    }
    // The drain. A reconciled id is gone from KHook's association, but the insert thread may
    // still push its `(id, details)` back after a failed capsule `try_lock`
    // (`detour.cpp:1618-1622`), and those details carry this module's removal function and a
    // context we deliberately keep allocated. On Linux Metamod `dlclose`s the plugin after
    // `Unload`, so that stale entry could later call into an unmapped image; one more
    // synchronous removal per lost id either erases the stale entry and calls the removal
    // function (which [`release_on_removal`] ignores, because the id is in [`Registry::lost`])
    // or takes the "not associated" no-op path (`detour.cpp:1582-1585`). It shrinks the window
    // to one insert-thread step rather than closing it (nothing of ours can close KHook's
    // window), which is why the context stays allocated for the process's life.
    //
    // The ids are permanent, so every drain re-issues all of them (a stale entry may be
    // re-queued at any time); only the *report* is once per id, from `Registry::drained`.
    let lost: Vec<HookId> = registry().lost.iter().copied().collect();
    if !lost.is_empty() {
        for id in &lost {
            // SAFETY: as in the loop above; a lost id is no hook KHook holds any more, and the
            // call is the same no-op/scan a removal of any unassociated id is.
            unsafe {
                api.remove_hook(
                    *id,
                    false,
                    ptr::null_mut::<c_void>(),
                    ptr::null_mut::<c_void>(),
                )
            };
        }
        let freshly_drained = {
            let mut registry = registry();
            lost.iter()
                .filter(|id| registry.drained.insert(**id))
                .count()
        };
        if freshly_drained > 0 {
            emit_diagnostic(&format!("khook: drained {freshly_drained} lost id(s)"));
        }
    }
    if live > 0 {
        // The sanity check: after re-issuing every id synchronously and reconciling every one
        // KHook lost, the count has to be zero.
        let (registry, _) = REGISTRY_CHANGED
            .wait_timeout_while(registry(), deadline, |r| r.live > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.live != 0 {
            return Err(Error::UnloadTimeout {
                remaining: registry.live,
            });
        }
    }
    if core::mem::take(&mut registry().unbarriered_removal) {
        // The barrier, before the `Ok` below claims that every removal function has returned.
        // [`release_on_removal`] lowers the count *inside* the removal function, so a count of
        // zero can be observed while that function's signal and its return are still ahead of it
        // on KHook's thread. One more synchronous removal for an id KHook does not hold waits for
        // exactly that: `RemoveHook` takes `insert_hooks_mutex` (the not-yet-inserted scan,
        // `detour.cpp:1819`) and then `associated_hooks_mutex` **exclusively**
        // (`__RemoveHook_Sync`, `:1581-1589`), which between them are held by every path that
        // calls a removal function of this module (`:1581` synchronous and delete-thread,
        // `:1819` the queue scan, `:1885-1888` `Shutdown`, whose capsule destructor runs them at
        // `:1429-1437`), and it runs nothing because [`INVALID_HOOK`] is not a hook id (KHook
        // hands ids out from a counter starting at 0, `detour.cpp:1729`; and even if such an id
        // existed, removing it is what this function exists to do). It never reaches a capsule's
        // own mutex, so it does not spin on a hooked call that is still on some thread's stack.
        //
        // **One path is not covered by this barrier.** Another plugin's `__Setup__Hook` can
        // replace a capsule through `insert_or_assign` (`detour.cpp:1701`), and destroying the
        // old capsule runs its removal functions (`:1429-1437`) while holding only
        // `hooks_detour_mutex`, which this barrier does not take (it is not ours to take: the
        // call below is `RemoveHook`, not a setup). A removal function of this module cannot be
        // in that path for an id this call still knows — its capsule belongs to a registration
        // of ours, and that other setup is a registration on the same address racing ours — but
        // `Ok` cannot be read as a proof against it either. The gate keeps *our* setups and
        // removals serialised; a foreign one is outside this module's control.
        //
        // SAFETY: as in the loop above; `INVALID_HOOK` names no hook of this module.
        unsafe {
            api.remove_hook(
                INVALID_HOOK,
                false,
                ptr::null_mut::<c_void>(),
                ptr::null_mut::<c_void>(),
            )
        };
    }
    Ok(())
}

/// How KHook installs a hook: the `async` argument of `IKHook::SetupHook` and
/// `IKHook::SetupVirtualHook` (`khook.hpp:2186-2187`).
///
/// The generated `setup_inline` / `setup_virtual` take one of these; pass [`SetupMode::Async`]
/// (the default, and what the C++ templates do). Neither mode is a hazard: KHook inserts
/// synchronously when it can and queues the work otherwise, in both directions
/// (`detour.cpp:1712-1717` forces `async = false` for the first hook on a fresh address, but that
/// insert can still fail — the address is patched before the capsule lock is released at
/// `:1705,1710`, so a call can be in flight — and `:1738-1743` then queues it; `:1745-1748`
/// queues the asynchronous one for the insert thread). What follows from that is that `Sync` does
/// **not** mean "the hook is live when this returns" — only that KHook was asked to try.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SetupMode {
    /// `async = true`: the insert thread installs the hook unless KHook decides to insert it
    /// synchronously anyway (`detour.cpp:1712-1717,1745-1748`). What the C++ templates pass "for
    /// safety reasons" (`khook.hpp:685`): it keeps `SetupHook` from holding the hooked address's
    /// mutex while the caller may be inside a hooked call.
    #[default]
    Async,
    /// `async = false`: ask KHook to insert the hook inside the `SetupHook` call
    /// (`detour.cpp:1738-1743`). If it cannot take the capsule's mutex it queues the hook for the
    /// insert thread instead and still returns a live id (`detour.cpp:1745-1748`), so the hook
    /// may not be in place yet when `setup_inline`/`setup_virtual` returns. Only *removal* waits
    /// for the mutex (`detour.cpp:1596`).
    Sync,
}

impl SetupMode {
    /// The header's `async` flag for this mode.
    #[must_use]
    pub const fn is_async(self) -> bool {
        matches!(self, Self::Async)
    }
}

/// The five addresses KHook stores per registration (`khook.hpp:2186`): the four thunks the
/// trampoline calls, plus the removal function it calls when the hook goes away.
///
/// Raw addresses on purpose: their signatures are the hooked function's, which only the macro
/// that generated them knows.
#[derive(Clone, Copy, Debug)]
pub struct Thunks {
    /// Called before the original (`detour.cpp:873-880`).
    pub pre: *mut c_void,
    /// Called after it (`detour.cpp:931-940`).
    pub post: *mut c_void,
    /// Produces the value when a callback overrode it (`detour.cpp:963-973`).
    pub make_return: *mut c_void,
    /// Calls the original when nobody superseded (`detour.cpp:895-908`).
    pub make_call_original: *mut c_void,
    /// Passed as `SetupHook`'s `removed_function`; KHook calls it with the context pushed
    /// (`detour.cpp:1536-1541`).
    pub removed: *mut c_void,
}

/// Register a hook on a function address (`IKHook::SetupHook`), taking ownership of the
/// context.
///
/// # Sharing an address with another plugin
///
/// The trampoline keeps one `make_return` and one `make_call_original` per hooked address,
/// taken from whichever callback list is first (`detour.cpp:267-271`), and a hook that is not
/// first still runs its `pre`/`post`. If another plugin registered the same address first with
/// a different declaration (void against `u32`, say), this registration's `make_return` is not
/// the one that runs. Nothing here can detect that; the `debug_assert` on the value pointer in
/// the generated `make_return` is what a wrong declaration trips in a debug build.
///
/// An address's copy stack is likewise fixed by whoever registers it **first**: KHook creates
/// the `DetourCapsule` from that registration's `stack_size` and passes later ones the existing
/// capsule, ignoring their argument (`detour.cpp:1696-1719`; the capsule rounds the number up to
/// 16 at `:535-536`). Registrations through *this* module are checked against each other — one
/// needing more than the capsule was created for is refused with
/// [`Error::CapsuleStackTooSmall`] — but a capsule another plugin created is invisible here, so
/// a smaller one from outside cannot be detected. Register the largest signature you hook on an
/// address first, or let this module be the one to do it.
///
/// # Errors
///
/// [`Error::GateBusy`] when called from inside a hook thunk of this module and another thread
/// held the hook gate at that moment: from a thunk the gate is taken with `try_lock`, never
/// with a blocking wait (a holder may be spinning in a synchronous removal on the capsule
/// this call holds shared, `detour.cpp:214,1503,1596`; see [`Error::GateBusy`]), and the
/// refusal costs nothing because no KHook call is made. Nothing was registered and the
/// context is freed exactly as on every other error; retry the registration on a later tick.
/// With the gate free a registration from a thunk proceeds as anywhere else — its KHook calls
/// never wait on a capsule. [`Error::KHookNotInstalled`] with no API installed,
/// [`Error::HookSetupFailed`] when KHook answers [`INVALID_HOOK`],
/// [`Error::CapsuleStackTooSmall`] when this address's capsule (see above) was created for
/// fewer bytes than this signature needs — in all of these cases the context is freed here
/// (no removal function will ever run for it) and no live [`Hook`] is produced. The caller's
/// reference to `context` is always consumed, error or not.
///
/// # Safety
///
/// `target` must be a live function with exactly the signature and ABI the four thunks were
/// generated for; `context` must be the `Box` allocation of that generator's context type
/// (with a [`CtxHeader`] first) and `thunks` the matching addresses.
pub unsafe fn register_inline(
    target: *mut c_void,
    context: *mut c_void,
    thunks: Thunks,
    stack_size: u32,
    mode: SetupMode,
) -> Result<Hook> {
    // G1: one KHook setup or removal of this module at a time (see [`KHOOK_GATE`]). The
    // capsule claim and `finish_registration` take `REGISTRY` under it, so the lock order is
    // `KHOOK_GATE -> REGISTRY`.
    //
    // From inside a thunk the gate is taken with `try_lock` and a held one answers
    // [`Error::GateBusy`]: a gate holder may be in a synchronous removal spinning on the
    // capsule this very call holds shared (`detour.cpp:214,1503,1596`), so waiting for it
    // could deadlock. The registration's own KHook calls never wait on a capsule — setup
    // takes `hooks_detour_mutex` exclusive for a fresh address (`detour.cpp:1698-1710`) and
    // shared across the insert (`:1722-1750`), takes the id and association mutexes briefly
    // and inserts with a `try_lock` (`:1453`), so a target on this thread's own stack just
    // goes to KHook's insert thread — so with the gate free a registration from a thunk is
    // deadlock-free (the `hooks_detour_mutex` exception is the G1 section of the module
    // docs).
    let _gate = if in_thunk() {
        match khook_gate_try() {
            Some(gate) => gate,
            None => {
                // The caller handed us its reference to the context; nothing can use it now,
                // and the caller has no way to take it back.
                //
                // SAFETY: the caller guarantees that reference is the only outstanding one.
                unsafe { release(context) };
                return Err(Error::GateBusy);
            }
        }
    } else {
        khook_gate()
    };
    let Some(api) = api() else {
        // The caller handed us its reference to the context; nothing can use it now, and the
        // caller has no way to take it back.
        //
        // SAFETY: the caller guarantees that reference is the only outstanding one.
        unsafe { release(context) };
        return Err(Error::KHookNotInstalled);
    };
    // KHook identifies a hooked inline function by its own address (`__Setup__Hook`'s
    // `unique_identifier`, `detour.cpp:1769-1781`), which is also the key of its capsule.
    let address = target as usize;
    // Whether *this* registration created the address's capsule, which decides whether a refusal
    // has to give that claim back ([`release_capsule`]).
    let created = match claim_capsule(address, stack_size) {
        Ok(created) => created,
        Err(err) => {
            // SAFETY: as above: the caller's reference is the only outstanding one.
            unsafe { release(context) };
            return Err(err);
        }
    };
    // KHook's reference and this module's live-context count are taken *before* the context
    // pointer is handed over. `SetupHook` inserts the hook inside the call when it can
    // (`detour.cpp:1738-1739` reaches `DetourCapsule::InsertHook`), so its callbacks can already
    // run — and, once the entry is in `_callbacks`, a capsule destruction runs its removal
    // function (`detour.cpp:1429-1437`) before this registration gets to record the id. That
    // cannot happen *inside* `__Setup__Hook` (it holds `hooks_detour_mutex` shared,
    // `:1722-1750`, while `Shutdown` needs it exclusive, `:1885`), but it can between
    // `setup_hook` returning and `finish_registration` taking the registry — which is why
    // `removed_before_registration` exists. Counting afterwards would be a write into a registry
    // whose entry a removal function had already lowered/removed.
    //
    // SAFETY: the caller guarantees `context` is a live context allocation whose only
    // outstanding reference is theirs.
    unsafe { retain(context) };
    count_registration();
    // SAFETY: the caller's contract covers the target and the thunks; the context is now kept
    // alive by the reference taken above.
    let id = unsafe {
        api.setup_hook(
            target,
            context,
            thunks.removed,
            thunks.pre,
            thunks.post,
            thunks.make_return,
            thunks.make_call_original,
            stack_size,
            mode.is_async(),
        )
    };
    if id == INVALID_HOOK && created {
        // KHook erased the capsule it had just created for this address when its setup failed
        // (`detour.cpp:1706`), so it is gone from `hooks_detour` and a later registration
        // starts again from its own stack size.
        release_capsule(address);
    }
    // SAFETY: as above.
    unsafe { finish_registration(id, context, false) }
}

/// Register a hook on a vtable entry (`IKHook::SetupVirtualHook`).
///
/// # Errors
///
/// As [`register_inline`], including [`Error::GateBusy`] from inside a thunk with the gate
/// held and [`Error::CapsuleStackTooSmall`]: KHook identifies a virtual hook by the *slot's*
/// address (`vtable.offset(index)`, `detour.cpp:1797`), so the two entries of one vtable are
/// two addresses with their own capsules.
///
/// # Safety
///
/// `vtable` and `index` must name a live virtual function of the hooked class, with the
/// signature the thunks were generated for, and the same covers `context`/`thunks`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn register_virtual(
    vtable: *mut *mut c_void,
    index: i32,
    context: *mut c_void,
    thunks: Thunks,
    stack_size: u32,
    mode: SetupMode,
) -> Result<Hook> {
    // G1: as in [`register_inline`] — from inside a thunk the gate is a `try_lock` that
    // answers [`Error::GateBusy`] when it is held; outside one it blocks.
    let _gate = if in_thunk() {
        match khook_gate_try() {
            Some(gate) => gate,
            None => {
                // As in `register_inline`: the caller's reference to the context is consumed
                // here.
                //
                // SAFETY: the caller guarantees that reference is the only outstanding one.
                unsafe { release(context) };
                return Err(Error::GateBusy);
            }
        }
    } else {
        khook_gate()
    };
    let Some(api) = api() else {
        // As in `register_inline`: the caller's reference to the context is consumed here.
        //
        // SAFETY: the caller guarantees that reference is the only outstanding one.
        unsafe { release(context) };
        return Err(Error::KHookNotInstalled);
    };
    // KHook's `unique_identifier` for a virtual hook: the *slot's* address, `vtable + index`
    // (`detour.cpp:1797`). Pointer arithmetic, not a hand-written offset: `wrapping_add` scales
    // by the pointee's size exactly as the C++ `vtable + index` does, and `index` may be
    // negative (it then wraps, like the C++ pointer arithmetic would).
    let address = vtable.wrapping_add(index as usize) as usize;
    let created = match claim_capsule(address, stack_size) {
        Ok(created) => created,
        Err(err) => {
            // SAFETY: as above: the caller's reference is the only outstanding one.
            unsafe { release(context) };
            return Err(err);
        }
    };
    // As in `register_inline`: KHook's reference and the live-context count are taken before
    // the pointer is handed over.
    //
    // SAFETY: the caller guarantees `context` is a live context allocation whose only
    // outstanding reference is theirs.
    unsafe { retain(context) };
    count_registration();
    // SAFETY: the caller's contract covers the vtable and the thunks; the context is now kept
    // alive by the reference taken above.
    let id = unsafe {
        api.setup_virtual_hook(
            vtable,
            index,
            context,
            thunks.removed,
            thunks.pre,
            thunks.post,
            thunks.make_return,
            thunks.make_call_original,
            stack_size,
            mode.is_async(),
        )
    };
    if id == INVALID_HOOK && created {
        // KHook erased the capsule it had just created for this address when its setup failed
        // (`detour.cpp:1706`), so it is gone from `hooks_detour` and a later registration
        // starts again from its own stack size.
        release_capsule(address);
    }
    // SAFETY: as above.
    unsafe { finish_registration(id, context, true) }
}

/// Count a registration KHook is being handed: the context it may now call into
/// ([`Registry::live`]). [`uncount_registration`] undoes it for a registration KHook refused.
fn count_registration() {
    registry().live += 1;
}

/// Undo [`count_registration`] when KHook answered [`INVALID_HOOK`]: no removal function will
/// ever run for that context, so nothing else can lower the count.
fn uncount_registration() {
    let mut registry = registry();
    match registry.live.checked_sub(1) {
        Some(live) => registry.live = live,
        None => {
            debug_assert!(
                false,
                "hook context count underflowed while undoing a refused registration"
            );
        }
    }
}

/// The successful/refused tail shared by both registration paths.
///
/// # Safety
///
/// `context` must be a live context allocation whose only reference is the caller's, and
/// [`count_registration`] must have counted this registration.
unsafe fn finish_registration(
    id: HookId,
    context: *mut c_void,
    virtual_hook: bool,
) -> Result<Hook> {
    if id == INVALID_HOOK {
        // Nothing was registered: KHook never took the reference that was taken for it, no
        // removal function will run, and no Hook is built — so both outstanding references are
        // ours to release and the registration is ours to uncount.
        uncount_registration();
        // SAFETY: two references are outstanding (the creator's and KHook's).
        unsafe { release(context) };
        // SAFETY: as above.
        unsafe { release(context) };
        return Err(Error::HookSetupFailed { virtual_hook });
    }
    // KHook holds its reference, and may call this context's thunks, until it calls the removal
    // function; `remove_all_blocking` is what makes KHook run it before an unload. The id also
    // enters `tracked` in the same lock scope: from here until the removal function runs, this
    // module knows KHook owes that id a removal ([`reconcile_lost`]).
    {
        let mut registry = registry();
        // ... unless KHook already ran the removal function after `setup_hook` returned and
        // before this point: `__Setup__Hook` releases `hooks_detour_mutex` before it returns
        // (`detour.cpp:1750`), so a concurrent `Shutdown` (`:1885`, exclusive) can destroy the
        // capsule and run every removal function it holds (`:1429-1437`), and another plugin's
        // `insert_or_assign` (`:1701`) can do the same by replacing the capsule. That call noted
        // the id and lowered the count (`Registry::removed_before_registration`); recording it
        // here as well would account for the same context a second time.
        if registry.removed_before_registration.remove(&id) {
            return Ok(Hook {
                id,
                context: NonNull::new(context)
                    .expect("a hook context comes from Box::into_raw and is never null"),
            });
        }
        registry.installed.insert(id, ());
        registry.tracked.insert(id);
    }
    Ok(Hook {
        id,
        context: NonNull::new(context)
            .expect("a hook context comes from Box::into_raw and is never null"),
    })
}

/// A live hook: owns the hook id and the context, and removes the hook in [`Drop`].
///
/// `!Send + !Sync` (it holds a raw pointer): like the C++ templates' hook objects, keep it on
/// the thread that installed it.
///
/// # Removal is asynchronous
///
/// Dropping it calls `RemoveHook(id, async = true, ..)` — the same choice the C++ templates
/// make (`khook.hpp:673`) — so the call only queues the removal and KHook's delete thread runs
/// the removal function later (`detour.cpp:1630-1669`). The context therefore outlives the
/// [`Hook`] in the common case; the reference count is what keeps it alive until then, and a
/// deferred callback can still run in that window. The id stays in this module's set of
/// removals KHook has not run yet, which is what [`remove_all_blocking`] re-issues
/// synchronously before an unload instead of waiting for that thread to get to it.
///
/// [`Hook::remove_sync`] is the explicit synchronous alternative for a target that is certainly
/// not on the current call stack. Both take [`KHOOK_GATE`], so two removals or setups of this
/// module never reach KHook at the same time.
///
/// # Deadlock hazard: do not drop a `Hook` from a hook callback
///
/// KHook's trampoline takes the hooked function's per-target mutex **shared** for the whole
/// call (`detour.cpp:214`) and releases it in `DestroyReturnValue` (`:432`) or on the
/// no-callback exit (`:183`); removal needs it **exclusive** and spins until it gets it
/// (`detour.cpp:1503` `try_lock` inside `:1581-1589`'s `while (true)`). A *synchronous* removal
/// of a target that is on the current call stack therefore waits forever — that is the same
/// hazard as in a callback that drops its own [`Hook`], or a per-frame callback that runs
/// teardown code which unloads hooks on that frame function, which is why both the C++ templates
/// and this `Drop` ask for the asynchronous removal ("For safety reasons", `khook.hpp:685`).
/// Asynchronous removal trades it for work on KHook's delete thread, which
/// [`live_context_count`]/[`remove_all_blocking`] exist to finish before an unload — by
/// re-issuing the removals synchronously, not by waiting for that thread.
///
/// A `Drop` from inside a thunk skips [`KHOOK_GATE`] (see the implementation): a gate holder may
/// itself be spinning on this call's capsule, so waiting for it would deadlock. If KHook's
/// delete thread then never runs that removal — it is lost to the insert-thread race — the
/// context stays counted by [`live_context_count`] until a [`remove_all_blocking`] re-issues it
/// synchronously and reconciles it.
///
/// # What `Drop` does, and does not, ask KHook for
///
/// It asks for `RemoveHook(id, async = true, ..)` only when this module still owes the id a
/// removal — `installed`, `pending` or `tracked` holds it, the same test [`Hook::remove_sync`]
/// makes. An id in none of those sets is one KHook has already finished with (an earlier
/// [`remove_all_blocking`], or its `Shutdown`), and the call would only be a "not associated"
/// no-op (`detour.cpp:1582-1585`); it is skipped.
///
/// An id reconciled as lost (`Registry::lost`) asks for nothing **and** releases nothing: it is
/// not KHook's any more, and its context — KHook's reference and this `Hook`'s — is leaked on
/// purpose (see [`reconcile_lost`]). Dropping the `Hook` a [`remove_all_blocking`] just
/// reconciled is therefore harmless and cannot free a context a stale re-queued entry may still
/// name.
///
/// With no API installed there is nobody to ask: the registration is left behind (KHook keeps
/// the hook and its own reference, and nobody can ask for its removal), and a diagnostic line is
/// emitted. This `Hook`'s own reference is still released — KHook's keeps the context alive, so
/// nothing KHook can call into is freed; what is lost is the removal. That is deliberate, because
/// this `Drop` can run inside a thunk — an `extern "C"` frame — where a panic would abort the
/// game.
///
/// Register hooks from ordinary game-thread code (Metamod `Load`/`Unload`, a shutdown path)
/// where you can. A registration from inside a callback is allowed — its KHook calls never
/// wait on a capsule — but it takes the hook gate with `try_lock` there and answers
/// [`Error::GateBusy`] when the gate is held, so it can fail where a registration from outside
/// would only have waited; a `Drop` from inside one asks for the asynchronous removal.
pub struct Hook {
    id: HookId,
    context: NonNull<c_void>,
}

impl Hook {
    /// The id KHook registered this hook under.
    #[must_use]
    pub fn id(&self) -> HookId {
        self.id
    }

    /// Remove this hook **synchronously** and say what happened to it.
    ///
    /// `Drop` asks KHook for an asynchronous removal (`khook.hpp:673`), because a synchronous
    /// one of a target that is on the current call stack spins for the hooked address's mutex
    /// until that call returns (`detour.cpp:1503,1581-1589,1871`). This is the opt-in for the
    /// paths that know the target is not on any stack: it asks for `RemoveHook(id,
    /// async = false, ..)` and returns once KHook's answer is in.
    ///
    /// It takes the process-wide gate for the whole call ([`KHOOK_GATE`]), so it does not race
    /// another setup or removal of this module, and it reconciles the lost-removal case
    /// ([`reconcile_lost`]): KHook can return from a synchronous removal without having called
    /// the removal function because its insert thread had the id in the window between
    /// `insert_hooks` and the capsule. That is reported as [`Removal::ReconciledLost`], the
    /// context is deliberately leaked (a stale re-queued entry may still name it) and the
    /// module counts the id in [`lost_removal_count`].
    ///
    /// From inside a hook thunk ([`in_thunk`]) it does **not** spin, in any build: it hands the
    /// hook to the asynchronous path [`Drop`] takes and answers [`Removal::Deferred`].
    /// [`in_thunk`] cannot tell "the target is on this stack" (a caller bug) from "a legitimate
    /// op running in another hook's tick", and an asynchronous removal never waits on a capsule
    /// or on the gate, so the fallback is always the safe answer — waiting for a capsule mutex
    /// its own call holds shared would hang the game thread for ever.
    ///
    /// # Errors
    ///
    /// [`Error::KHookNotInstalled`] when there is no API: the hook cannot be removed then, and
    /// the `Hook` is leaked rather than run through a `Drop` that would leak it too. (Leaking it
    /// is also what keeps the context — which KHook may still call — alive.) This is checked
    /// **first**, before the in-thunk fallback: from a thunk the fallback is `Drop`, whose job in
    /// that state is the same leak, and the error is the answer a caller in a thunk must see.
    ///
    /// # Ownership
    ///
    /// Always consumes the `Hook` without running its [`Drop`], so exactly one removal is asked
    /// of KHook. The creator's reference to the context is released on the normal path (as
    /// `Drop` does after its asynchronous request); on [`Removal::ReconciledLost`] it is
    /// **not** released, because the context is leaked on purpose (see [`reconcile_lost`]).
    pub fn remove_sync(self) -> Result<Removal> {
        // Checked before the in-thunk fallback below: `Drop` cannot remove the hook either, and
        // the error is what the caller in a thunk must see as the answer.
        if api().is_none() {
            core::mem::forget(self);
            return Err(Error::KHookNotInstalled);
        }
        if in_thunk() {
            // The call that is on this stack holds its capsule's mutex shared, so a synchronous
            // removal would spin on it for ever — and `in_thunk` cannot tell "the target is on
            // this stack" (a caller bug) from "an op legitimately running in another hook's
            // tick". Every build defers: `Drop` is the same asynchronous request a caller
            // outside a thunk would make, and it never waits on the capsule or the gate.
            drop(self);
            return Ok(Removal::Deferred);
        }
        // G1: this is one of the calls KHook must only see one of at a time from this module.
        let _gate = khook_gate();
        let Some(api) = api() else {
            // The API went away between the check above and the gate. Nothing can remove the
            // hook, so there is no safe `Drop` to fall back to: it would leak for the same
            // reason while the context KHook may still call has to stay.
            core::mem::forget(self);
            return Err(Error::KHookNotInstalled);
        };
        let id = self.id;
        let context = self.context.as_ptr();
        // The three sets are read and (for `installed`) updated in one lock scope. KHook may
        // still have the hook if it is in any of them; if it is in none, an earlier removal
        // (a `remove_all_blocking`, or a `Shutdown`) already ran the removal function and the
        // `RemoveHook` below would only be a "not associated" no-op.
        let (known, lost) = {
            let mut registry = registry();
            let was_installed = registry.installed.remove(&id).is_some();
            (
                was_installed
                    || registry.pending.contains_key(&id)
                    || registry.tracked.contains(&id),
                registry.lost.contains(&id),
            )
        };
        if !known {
            if lost {
                // An earlier synchronous removal already reconciled this id. Nothing is KHook's
                // any more and the context was leaked on purpose, so the `Hook`'s reference goes
                // with it rather than freeing an allocation the reconcile decided to keep.
                core::mem::forget(self);
                return Ok(Removal::ReconciledLost);
            }
            // KHook is finished with this hook already: nothing to ask, nothing to wait for.
            // The creator's reference is the only one left, so releasing it frees the context.
            // SAFETY: this is the reference the registration took for the `Hook`.
            unsafe { release(context) };
            core::mem::forget(self);
            return Ok(Removal::AlreadyGone);
        }
        // SAFETY: `id` is a live hook id of this module's registry (the `Hook` proves it was a
        // successful registration on this API), and the removal function KHook stored at setup
        // is the one it calls, so both caller-supplied slots are null (`khook.hpp:2188`,
        // `detour.cpp:1812-1877`).
        unsafe {
            api.remove_hook(
                id,
                false,
                ptr::null_mut::<c_void>(),
                ptr::null_mut::<c_void>(),
            )
        };
        // The synchronous call returned: any removal function KHook could still have run for
        // this id has retired (proof on [`reconcile_lost`]), so an id still in `tracked` is one
        // KHook lost.
        let outcome = {
            let mut registry = registry();
            reconcile_lost(&mut registry, id)
        };
        if report_reconcile(id, outcome) {
            // The context is leaked on purpose: a stale re-queued `insert_hooks` entry may still
            // name it, and `release_on_removal` ignores the id rather than touching the pointer.
            // The `Hook`'s own reference goes with it, exactly like KHook's.
            core::mem::forget(self);
            return Ok(Removal::ReconciledLost);
        }
        // KHook removed it: the removal function ran (or the id was already gone from the
        // capsule) and released KHook's reference. This is the creator's, exactly as `Drop`
        // releases it after asking for the asynchronous removal.
        // SAFETY: this is the reference the registration took for the `Hook`.
        unsafe { release(context) };
        core::mem::forget(self);
        Ok(Removal::Removed)
    }
}

/// What [`Hook::remove_sync`] found, and what it did about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// KHook removed the hook and ran the removal function: the context is freed (the `Hook`
    /// and KHook held the last references).
    Removed,
    /// KHook had already finished with the hook and it was never reconciled as lost (an earlier
    /// [`remove_all_blocking`], or its `Shutdown`): nothing was asked of it and the context was
    /// freed by the `Hook`'s reference alone. A hook that *was* reconciled earlier answers
    /// [`Removal::ReconciledLost`] instead and keeps its context leaked.
    AlreadyGone,
    /// KHook lost the hook to its insert-thread race and never called the removal function: the
    /// id was reconciled ([`reconcile_lost`]) and its context is **leaked** on purpose, so
    /// [`live_context_count`] is no longer counting it but the allocation stays for the life of
    /// the process.
    ReconciledLost,
    /// Called from inside a hook thunk, so the synchronous removal was refused and the hook was
    /// handed to the asynchronous path instead: the removal is now queued for KHook's delete
    /// thread exactly as [`Drop`] would have queued it, [`live_context_count`] still counts the
    /// context, and [`remove_all_blocking`] re-issues it before an unload.
    Deferred,
}

impl core::fmt::Debug for Hook {
    /// The id and the context address; there is nothing else to say about a live hook.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hook")
            .field("id", &self.id)
            .field("context", &self.context.as_ptr())
            .finish()
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        // G1: the removal is one of the calls KHook may only see one of at a time from this
        // module (see [`KHOOK_GATE`]). The registry bookkeeping below runs under the gate, so
        // the lock order is `KHOOK_GATE -> REGISTRY`.
        //
        // **A drop from inside a thunk skips the gate.** The call on this thread's stack holds
        // its capsule's `_detour_mutex` shared (`detour.cpp:214`), and that is exactly the mutex
        // a `remove_sync`/`remove_all_blocking` holding the gate may be spinning on for this id
        // (`detour.cpp:1503,1596`): waiting for the gate here would leave both waiting for each
        // other. Skipping it is sound because this drop only ever asks for the **asynchronous**
        // removal, which never waits on a capsule: `RemoveHook(id, async = true)` takes
        // `insert_hooks_mutex` for its scan and `associated_hooks_mutex` shared for the
        // association check and the queue push (`detour.cpp:1819-1850,1852-1869`), and never
        // blocks on a capsule's own mutex. Our scan is serialised with a gate holder's by
        // `insert_hooks_mutex`, and KHook's delete-thread pass by `associated_hooks_mutex`
        // exclusive (`__RemoveHook_Sync`, `:1581-1589`), so the registry bookkeeping below still
        // sees a consistent answer from KHook. The one thing it can do is add a `pending` id
        // after a `remove_all_blocking` took its snapshot; that function's `live` wait covers it
        // (the delete thread runs it) or times out (`UnloadTimeout`), which its docs say.
        let _gate = if in_thunk() { None } else { Some(khook_gate()) };
        // Where the id stands decides whether KHook still has anything to remove at all. The
        // registry bookkeeping happens *before* the call, because KHook can run the removal
        // function inside it — a hook that is still in its `insert_hooks` queue is removed by the
        // scan at the top of `RemoveHook` (`detour.cpp:1819-1850`), whatever the `async` flag —
        // and that has to find the id out of `installed` and in `pending` (it also takes this
        // same lock).
        let (known, lost) = {
            let mut registry = registry();
            let was_installed = registry.installed.remove(&self.id).is_some();
            if was_installed {
                // KHook still holds this hook, so its removal is now queued: the id belongs in
                // the set of removals KHook has not run yet, which a later
                // `remove_all_blocking` re-issues synchronously. It can also hold a `Drop`'s id
                // whose removal was never issued because no API was installed — the same set,
                // re-issued once an API is back.
                registry.pending.insert(self.id, ());
            }
            // Whether KHook can still hold the id: still registered, queued for the delete
            // thread, or owed a removal. If it is in none of them KHook is finished with the
            // hook (an earlier `remove_all_blocking`, or a `Shutdown`) and the `RemoveHook` below
            // would only be a "not associated" no-op — the same test [`Hook::remove_sync`]
            // makes.
            (
                was_installed
                    || registry.pending.contains_key(&self.id)
                    || registry.tracked.contains(&self.id),
                registry.lost.contains(&self.id),
            )
        };
        if lost {
            // Reconciled as lost: KHook does not hold the id any more and its context was
            // leaked on purpose (see [`reconcile_lost`]). There is no removal to ask for, and
            // this reference must not be released — the docs' "never released, not KHook's,
            // not the Hook's" — whatever the API state.
            return;
        }
        // No API: the removal cannot be asked for, and this Drop may be running inside a thunk (a
        // callback dropping its own Hook), where a panic is a non-unwinding abort. Report it, and
        // release **this** reference: KHook's still keeps the context alive (and the removal
        // releases that one if KHook ever runs it), so nothing KHook may call into is freed; if
        // KHook was already finished with the hook, this frees it, exactly as the normal path
        // would. What leaks is the registration — but only when `known`: when the id was in none
        // of the registry sets KHook is already done with it, this release is the last reference,
        // and "the registration is leaked" would be false.
        let Some(api) = api() else {
            if known {
                emit_diagnostic(&missing_api_drop_line(self.id));
            }
            // SAFETY: the registration took this reference (`retain` in `finish_registration`)
            // and it is released exactly once, here.
            unsafe { release(self.context.as_ptr()) };
            return;
        };
        if known {
            // SAFETY: `id` came from a successful registration on this same API; the removal
            // function KHook stored at setup is the one it calls, so both caller-supplied slots
            // are null (`khook.hpp:2188`, `detour.cpp:1812-1877`).
            unsafe {
                api.remove_hook(
                    self.id,
                    true,
                    ptr::null_mut::<c_void>(),
                    ptr::null_mut::<c_void>(),
                )
            };
        }
        // The asynchronous removal above (when it was made) has only queued our removal thunk
        // (KHook's delete thread runs it, `detour.cpp:1865-1867`), so KHook still holds its
        // reference and this release cannot be the last one — except when KHook was already
        // finished with the hook (a `Shutdown`, or an earlier `remove_all_blocking`), in which
        // case this *is* the last reference and the context is freed here, on this thread.
        // SAFETY: the registration took this reference (`retain` in `finish_registration`)
        // and it is released exactly once, here.
        unsafe { release(self.context.as_ptr()) };
    }
}

// ---------------------------------------------------------------------------------------
// Thunk depth: is this thread inside a hook thunk?
// ---------------------------------------------------------------------------------------

thread_local! {
    /// How many `pre`/`post` thunks of this module are running on this thread. A counter rather
    /// than a flag because a callback may re-enter the hooked function (or call another hooked
    /// function), which nests thunks on the same thread; the guard type keeps every entry paired
    /// with an exit.
    static THUNK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Is the calling thread inside a `pre`, `post` or `make_call_original` thunk of this module?
///
/// `true` means the current stack is inside a hooked call (KHook's trampoline holds the hooked
/// address's mutex shared for it, `detour.cpp:214,1503`). Inside one, a **synchronous** removal
/// of that same target would spin for ever (`__RemoveHook_Sync(id, wait = true)` has no deadline,
/// `detour.cpp:1581-1589`), so [`Hook::remove_sync`] hands it to the asynchronous path [`Drop`]
/// takes ([`Removal::Deferred`], every build — [`in_thunk`] cannot tell "the target is on this
/// stack", a caller bug, from "a legitimate op running in another hook's tick") and
/// [`remove_all_blocking`] refuses it. A registration from
/// inside a thunk is allowed: its KHook calls never wait on a capsule (its capsule insert is a
/// `try_lock`, `detour.cpp:1453`), so it takes the hook gate with `try_lock` and answers
/// [`Error::GateBusy`] when the gate is held, rather than waiting on a holder that may be
/// spinning on this call's capsule. A caller that wants to tear hooks down from a per-frame
/// callback must use [`Drop`] (asynchronous) instead.
///
/// The depth only knows thunks *this module* generated. A hook another plugin owns is invisible
/// here: while a foreign hook's `pre` runs, [`in_thunk`] is `false`, even though that plugin's
/// call is in flight (and holds that hook's own capsule shared). A caller that has to know
/// whether *some* detour of the process is on the stack cannot ask this function; it is
/// documented for this module's own hooks, which is what the removal guard needs.
#[must_use]
pub fn in_thunk() -> bool {
    THUNK_DEPTH.with(Cell::get) > 0
}

/// The depth guard every generated `pre`/`post`/`make_call_original` thunk holds for the length
/// of its body (internal). Dropping it pairs the entry, including through a nested call or an
/// early return.
///
/// `!Send` on purpose (through the raw-pointer marker): the depth is a thread-local of the
/// thread KHook entered the thunk on, so carrying the guard to another thread and dropping it
/// there would decrement that thread's depth — it would "leave" a thunk that thread never
/// entered. It is `pub` only because the macro in every `khook_thunks!` module lives in the
/// caller's crate; it is not part of the wrapper's API.
#[doc(hidden)]
pub struct ThunkGuard(core::marker::PhantomData<*const ()>);

impl ThunkGuard {
    /// Enter a thunk on this thread.
    #[must_use]
    pub fn enter() -> Self {
        THUNK_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self(core::marker::PhantomData)
    }
}

impl Drop for ThunkGuard {
    fn drop(&mut self) {
        THUNK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

// ---------------------------------------------------------------------------------------
// The thunk macro
// ---------------------------------------------------------------------------------------

/// Build the five KHook entry points and the setup functions for one hooked signature.
///
/// See the module docs for the contract; this is the mechanical part. The declaration reads
/// like the hooked function itself, with `this` first (as KHook's `Member<>`/`Virtual<>`
/// thunks take it) and the vtable/inline choice made at setup:
///
/// ```
/// use core::ffi::c_void;
/// use s2sdk::khook_thunks;
///
/// khook_thunks! {
///     /// `void CBaseEntity::SomeMethod(u32)` — the hooked signature.
///     pub mod on_some_method: fn(this: *mut c_void, arg: val u32) -> () {
///         sret_msvc = no_sret,
///         sret_itanium = no_sret,
///     }
/// }
/// # fn main() {}
/// ```
///
/// # Every parameter carries a marker: `val` or `ref`
///
/// `this` is always the object pointer KHook's `Member<>`/`Virtual<>` thunks take first (`void*`
/// in the C++ stack-size computation, `khook.hpp:685`), so it takes no marker; every other
/// parameter declares what the *C++* parameter is, because that — not the Rust mirror's size —
/// is what `_copy_stack_size` measures (`khook.hpp:96-103`):
///
/// * `val` — the C++ parameter is by value, or is a pointer. `sizeof(ARG)` is then the size of
///   the Rust mirror itself (`size_of::<*const QAngle>()` = 8 for a `const QAngle*`).
/// * `ref` — the C++ parameter is a reference, written with the Rust pointer that mirrors it:
///   `ang: ref *const QAngle` for `const QAngle&`. `sizeof(T&)` is `sizeof(T)`, so this counts
///   `size_of::<QAngle>()` = 12 ([`CppReferent`]), which is what KHook copies for the referent.
///   A plain `*const QAngle` marked `val` would be 8 and would under-copy the stack. An rvalue
///   reference (`QAngle&&`) is the same size ([expr.sizeof]/2 again) and is spelled the same
///   way: `ref *mut QAngle`.
///
/// The marker never reaches the generated thunks, the callback types or `make_call_original`:
/// the machine signature is the declared Rust type, `this` first, exactly as KHook calls it.
/// Anything other than `val`/`ref` is a compile error naming the marker
/// ([`__khook_arg_size`]).
///
/// A C++ **reference return** needs nothing of its own: KHook cannot express one either
/// (`RETURN* _fake_return`, `khook.hpp:109`, with `new RETURN` at `:112` and the
/// `Return<RETURN>` machinery at `:1333`, is ill-formed for `RETURN = T&`), so a C++ plugin
/// hooks `T& f()` by declaring `T* f()` — one pointer-sized result, travelling in `RAX` — and
/// this macro's mirror of that is `-> *mut T`, whose `size_of` is exactly the pointer slot the
/// two stack-size formulas count. There is no hole, and the declared-vs-computed stack-size
/// check ([`check_stack_size`]) is what catches a hook whose declared number disagrees.
///
/// and then
///
/// ```no_run
/// # use core::ffi::c_void;
/// # use s2sdk::hook::{HookCall, Return, SetupMode};
/// # use s2sdk::khook_thunks;
/// # khook_thunks! {
/// #     pub mod on_some_method: fn(this: *mut c_void, arg: val u32) -> () {
/// #         sret_msvc = no_sret,
/// #         sret_itanium = no_sret,
/// #     }
/// # }
/// # fn main() {
/// # let vtable = core::ptr::null_mut();
/// let hook = unsafe {
///     on_some_method::setup_virtual(
///         vtable,
///         12,
///         vec![std::sync::Arc::new(|_call: &mut HookCall,
///                                    _this: *mut c_void,
///                                    _arg: u32| {
///             Return::supersede_with(())
///         })],
///         Vec::new(),
///         None,
///         SetupMode::Async,
///     )
/// };
/// # let _ = hook;
/// # }
/// ```
///
/// The macro's items are generated into a module, so the types in the declaration have to
/// resolve in the module *containing the invocation* (it re-exports that scope with
/// `use super::*`); a fully qualified `*mut core::ffi::c_void` always works.
///
/// The macro asserts the return type is `Copy + Default` (KHook copies it into its own
/// storage, `khook.hpp:307-315,373-404`, and the thunk needs a dummy value to hand back in
/// the `pre`/`post` slots the trampoline ignores, `khook.hpp:762-765`):
///
/// ```compile_fail
/// use core::ffi::c_void;
/// use s2sdk::khook_thunks;
///
/// // error[E0277]: the trait bound `String: Copy` is not satisfied
/// khook_thunks! {
///     pub mod on_some_method: fn(this: *mut c_void) -> String {
///         sret_msvc = no_sret,
///         sret_itanium = no_sret,
///     }
/// }
/// # fn main() {}
/// ```
///
/// A parameter marker that is neither `val` nor `ref` is named in the error instead of being
/// read as a type:
///
/// ```compile_fail
/// use core::ffi::c_void;
/// use s2sdk::khook_thunks;
///
/// // error: unknown KHook parameter marker `value`: expected `val` ... or `ref` ...
/// khook_thunks! {
///     pub mod on_some_method: fn(this: *mut c_void, arg: value u32) -> () {
///         sret_msvc = no_sret,
///         sret_itanium = no_sret,
///     }
/// }
/// # fn main() {}
/// ```
///
/// `sret_msvc` / `sret_itanium` say whether the hooked function returns its value through a
/// hidden pointer on that ABI. Neither rule can be read off the Rust return type, which is
/// exactly why the flags exist and why they come from the generated IR — per signature, from
/// the method's `msvc_rust_fn` (`crates/s2sdk/s2sdk-sys/gen/gen.py:3319-3330`) or from whatever
/// const the generator emits for it — and are never chosen by hand:
///
/// * **MSVC** returns *every* class, struct or union from a non-static member function through
///   the hidden pointer, whatever its size: a 4-byte record comes back that way too
///   (`msvc_hidden_return_fn`, `gen/gen.py:3332-3360`, where clang spells it `sret(%T)` as the
///   second parameter and the callee hands the pointer back in `RAX`). MSVC's register-return
///   rule — 1-, 2-, 4- and 8-byte values in `RAX`, 16 bytes in `RAX:RDX` — is for non-class
///   types; a `#[repr(C)]` 4-byte *class* mirror is therefore `sret` on this ABI, not
///   `no_sret`.
/// * **Itanium** returns trivially-copyable values of at most 16 bytes in registers (three
///   floats are SSE class: `XMM0:XMM1`) and everything else through a hidden pointer — and a
///   *non-trivially-copyable* C++ value (one with a destructor or a copy constructor,
///   `CUtlString` for instance) goes through the pointer at any size. A `#[repr(C)]` Rust mirror
///   of such a class can still be `Copy` and `Default`, so it compiles here while the ABI flag
///   would be a lie: the flag, not the Rust type, has to be the truth.
///
/// When the flag is set for the target in use, the thunks take the caller's return buffer as
/// well: MSVC passes it *after* `this`, Itanium *before* it, and they return it
/// (`khook.hpp:87-91` for the why; `SaveReturnValue` is given the real value either way). Which
/// of the two shapes this build generated is readable as the module's
/// `HIDDEN_RETURN_BUFFER`.
///
/// A free function (KHook's `Function<>`, whose thunks have no `this` and whose hidden return
/// pointer would be at position zero) is a fourth shape, not a flag of this one: it is not
/// produced by this macro.
///
/// The generated `setup_inline`/`setup_virtual` take a [`SetupMode`] as their last argument:
/// [`SetupMode::Async`] (the default value, and what the C++ templates pass, `khook.hpp:685`)
/// unless the target is certainly not on any stack.
#[macro_export]
macro_rules! khook_thunks {
    (
        $(#[$meta:meta])*
        $vis:vis mod $name:ident : fn(this: $this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)* $(,)?) -> () {
            sret_msvc = $sm:tt,
            sret_itanium = $si:tt $(,)?
        }
    ) => {
        $crate::__khook_thunks_body! {
            $(#[$meta])* $vis $name,
            [$this_ty $(, $arg: $marker $aty)*],
            (),
            false,
            core::option::Option::None,
            [$sm] [$si]
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis mod $name:ident : fn(this: $this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)* $(,)?) -> $ret:ty {
            sret_msvc = $sm:tt,
            sret_itanium = $si:tt $(,)?
        }
    ) => {
        $crate::__khook_thunks_body! {
            $(#[$meta])* $vis $name,
            [$this_ty $(, $arg: $marker $aty)*],
            $ret,
            true,
            core::option::Option::Some(core::mem::size_of::<$ret>()),
            [$sm] [$si]
        }
    };
}

/// The body of [`khook_thunks!`] (internal): the voidness of the return is already decided,
/// as a bool for the Win64 count and an `Option` for the Linux one.
#[doc(hidden)]
#[macro_export]
macro_rules! __khook_thunks_body {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident,
        [$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*],
        $ret:ty,
        $non_void:expr,
        $linux_ret_size:expr,
        [$sm:tt] [$si:tt]
    ) => {
        $(#[$meta])*
        #[allow(missing_docs)] // the caller documents the module; the items are mechanical.
        $vis mod $name {
            // So the types in the declaration resolve exactly as the caller wrote them at the
            // call site (the generated module does not inherit them otherwise).
            #[allow(unused_imports)]
            use super::*;

            /// The hooked function's return type (`()` for a void one).
            type Ret = $ret;

            /// What a `pre` callback is: the call it is part of, `this`, the hooked
            /// function's arguments, and what KHook should do.
            ///
            /// `Fn`, not `FnMut`, and `Send + Sync`: KHook runs a hook on whichever thread
            /// reached the hooked function, and the callback list is cloned for each dispatch
            /// ([`s2sdk::hook::dispatch`]). State a callback keeps must therefore come with
            /// its own synchronisation (the C++ callbacks are plain function pointers, so they
            /// have no state at all to worry about — `khook.hpp:476-478`).
            pub type PreCb = std::sync::Arc<
                dyn Fn(
                        &mut $crate::hook::HookCall,
                        *mut core::ffi::c_void
                        $(, $aty)*
                    ) -> $crate::hook::Return<Ret>
                    + Send
                    + Sync
                    + 'static,
            >;

            /// What a `post` callback is (KHook gives both the same signature,
            /// `khook.hpp:768-774`).
            pub type PostCb = PreCb;

            /// The per-hook context KHook gets as its `context` and hands back through
            /// `GetContextPtr()`. `header` first so [`s2sdk::hook::release`] finds the
            /// reference count from the opaque pointer; the callbacks fold in the order the
            /// caller listed them (`khook.hpp:740-753`), each dispatch working on a clone
            /// taken under the mutex.
            #[repr(C)]
            struct Ctx {
                header: $crate::hook::CtxHeader,
                pre: std::sync::Mutex<Vec<PreCb>>,
                post: std::sync::Mutex<Vec<PostCb>>,
                fake_return: Ret,
            }

            // `khook.hpp:96-103` and `:83-95`.
            /// The stack size `_copy_stack_size` computes off `_WIN64` (`khook.hpp:96-103`).
            pub const STACK_SIZE_LINUX: u32 = $crate::hook::stack_size_linux(
                &[core::mem::size_of::<$this_ty>() $(, $crate::__khook_arg_size!($marker, $aty))*],
                $linux_ret_size,
            );

            /// The stack size `_copy_stack_size` computes on `_WIN64` (`khook.hpp:83-95`).
            pub const STACK_SIZE_WIN64: u32 = $crate::hook::stack_size_win64(
                $crate::hook::one_of::<$this_ty>()
                    $(+ $crate::hook::one_of::<$aty>())*,
                $non_void,
            );

            /// The computed stack size for the target this is compiled for.
            pub const STACK_SIZE: u32 =
                if cfg!(windows) { STACK_SIZE_WIN64 } else { STACK_SIZE_LINUX };

            // KHook copies the return value into its own storage through `init_op`
            // (`khook.hpp:307-310,373-404`) and a hook needs a dummy to hand back from the
            // `pre`/`post` slots the trampoline ignores (`khook.hpp:762-765`), so the return
            // type has to be `Copy` and have a `Default`.
            const _: fn() = || {
                fn assert_return_bounds<T: Copy + Default>() {}
                let _ = assert_return_bounds::<Ret>;
            };

            // KHook's reference to the context is released by the removal function KHook runs,
            // and that runs on KHook's delete thread for the asynchronous removal [`Drop`] and
            // [`setup_inline`]/[`setup_virtual`] ask for (`detour.cpp:1630-1669`), so the
            // context — the callbacks and the dummy return value — has to be freeable from
            // another thread: `Ret: Send` plus the already `Send + Sync` callbacks.
            const _: fn() = || {
                fn assert_send<T: Send>() {}
                let _ = assert_send::<Ctx>;
            };

            // `this` is the object pointer KHook's `Member<>`/`Virtual<>` thunks take first; the
            // C++ templates' stack-size computation counts it as one `void*` (`khook.hpp:685`),
            // and KHook enters the thunks with the hooked call's own registers. A `this` of any
            // other size would make the machine signature and the computed slot disagree.
            const _: () = assert!(
                core::mem::size_of::<$this_ty>()
                    == core::mem::size_of::<*mut core::ffi::c_void>(),
                "`this` must be pointer-sized: KHook's thunks take the hooked object as one \
                 `void*` argument (khook.hpp:685)"
            );

            // KHook's copy of the return value is skipped whenever the size it is told is 0
            // (`detour.cpp:373-380,397-404`), so a declared non-void return type must really
            // have a size. A C++ empty struct is one byte; a Rust zero-sized struct is not the
            // same declaration and this is where that is caught.
            const _: () = {
                if $non_void {
                    assert!(
                        core::mem::size_of::<Ret>() != 0,
                        "a non-void hook return type must not be zero-sized: KHook would not \
                         copy any return value for it"
                    );
                }
            };

            /// The five addresses KHook stores for this signature.
            pub const THUNKS: $crate::hook::Thunks = $crate::hook::Thunks {
                pre: pre as ThunkFn as *mut core::ffi::c_void,
                post: post as ThunkFn as *mut core::ffi::c_void,
                make_return: make_return as ThunkFn as *mut core::ffi::c_void,
                make_call_original: make_call_original as ThunkFn as *mut core::ffi::c_void,
                removed: removed as unsafe extern "C" fn($crate::hook::HookId)
                    as *mut core::ffi::c_void,
            };

            $crate::__khook_thunks_abi! { $sm, $si, [$this_ty $(, $arg: $marker $aty)*] }

            $crate::__khook_removed_fn! {}

            /// Inline-detour this signature onto `target` (`IKHook::SetupHook`): the returned
            /// [`s2sdk::hook::Hook`] removes it when dropped.
            ///
            /// `declared_stack_size` is the value a generated IR says the hook needs, checked
            /// against the computed one ([`s2sdk::hook::Error::StackSizeMismatch`]); pass
            /// `None` to trust the computation.
            ///
            /// `mode` is KHook's `async` argument: pass
            /// [`s2sdk::hook::SetupMode::Async`] (what the C++ templates pass,
            /// `khook.hpp:685`) unless the target is certainly not on any stack. Asking for
            /// [`s2sdk::hook::SetupMode::Sync`] does not guarantee the hook is live when this
            /// returns: KHook queues the insert when it cannot take the address's mutex
            /// (`detour.cpp:1738-1748`).
            ///
            /// # Errors
            ///
            /// [`s2sdk::hook::Error::KHookNotInstalled`],
            /// [`s2sdk::hook::Error::HookSetupFailed`],
            /// [`s2sdk::hook::Error::StackSizeMismatch`],
            /// [`s2sdk::hook::Error::CapsuleStackTooSmall`] when this target's hook capsule was
            /// created for a smaller stack size by an earlier registration on it, and
            /// [`s2sdk::hook::Error::GateBusy`] when called from inside a hook thunk of this
            /// module while another thread held the hook gate — from a thunk the gate is taken
            /// with `try_lock`, never waited for (retry the registration on a later tick).
            ///
            /// # Safety
            ///
            /// `target` must be a live function with exactly this declaration's signature and
            /// ABI.
            pub unsafe fn setup_inline(
                target: *mut core::ffi::c_void,
                pre: Vec<PreCb>,
                post: Vec<PostCb>,
                declared_stack_size: core::option::Option<u32>,
                mode: $crate::hook::SetupMode,
            ) -> $crate::hook::Result<$crate::hook::Hook> {
                let stack_size =
                    $crate::hook::check_stack_size(STACK_SIZE, declared_stack_size)?;
                let ctx = $crate::hook::ctx_alloc(Ctx {
                    header: $crate::hook::CtxHeader::new($crate::hook::drop_box::<Ctx>),
                    pre: std::sync::Mutex::new(pre),
                    post: std::sync::Mutex::new(post),
                    fake_return: <Ret as Default>::default(),
                });
                // SAFETY: the caller guarantees the target's signature; `ctx` is this module's
                // context with its header first; `THUNKS` are the thunks just generated.
                unsafe { $crate::hook::register_inline(target, ctx, THUNKS, stack_size, mode) }
            }

            /// Hook one vtable entry with this signature (`IKHook::SetupVirtualHook`); the
            /// index comes from the generated SDK, never from a number written by hand. The
            /// returned [`s2sdk::hook::Hook`] removes it when dropped.
            ///
            /// # Errors
            ///
            /// As [`setup_inline`].
            ///
            /// `mode` works as in [`setup_inline`]: [`s2sdk::hook::SetupMode::Async`] unless the
            /// vtable entry's function is certainly not on any stack.
            ///
            /// # Safety
            ///
            /// `vtable` must point at a live vtable whose `index`-th entry has exactly this
            /// declaration's signature and ABI.
            pub unsafe fn setup_virtual(
                vtable: *mut *mut core::ffi::c_void,
                index: i32,
                pre: Vec<PreCb>,
                post: Vec<PostCb>,
                declared_stack_size: core::option::Option<u32>,
                mode: $crate::hook::SetupMode,
            ) -> $crate::hook::Result<$crate::hook::Hook> {
                let stack_size =
                    $crate::hook::check_stack_size(STACK_SIZE, declared_stack_size)?;
                let ctx = $crate::hook::ctx_alloc(Ctx {
                    header: $crate::hook::CtxHeader::new($crate::hook::drop_box::<Ctx>),
                    pre: std::sync::Mutex::new(pre),
                    post: std::sync::Mutex::new(post),
                    fake_return: <Ret as Default>::default(),
                });
                // SAFETY: the caller guarantees the vtable entry's signature; `ctx` is this
                // module's context with its header first; `THUNKS` are the thunks just
                // generated.
                unsafe {
                    $crate::hook::register_virtual(vtable, index, ctx, THUNKS, stack_size, mode)
                }
            }
        }
    };
}

/// Which thunk shape this ABI needs (internal): the flags decide whether the hidden return
/// buffer is part of the signature, and where. Each arm also records the decision as the
/// module's [`HIDDEN_RETURN_BUFFER`], so a caller (and the tests) can see which of the two
/// machine signatures this build compiled.
#[doc(hidden)]
#[macro_export]
macro_rules! __khook_thunks_abi {
    (no_sret, no_sret, [$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*]) => {
        /// Whether this target's thunks take the caller's hidden return buffer: `false` here,
        /// so the machine signature is the declared one, `(this, args...) -> Ret`
        /// (`sret_msvc`/`sret_itanium` both say the value comes back in registers).
        pub const HIDDEN_RETURN_BUFFER: bool = false;

        $crate::__khook_thunks_plain! { [$this_ty $(, $arg: $marker $aty)*] }
    };
    (sret, no_sret, [$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*]) => {
        /// Whether this target's thunks take the caller's hidden return buffer: MSVC (the
        /// `sret_msvc` flag) does, Itanium (no `sret_itanium`) does not.
        #[cfg(windows)]
        pub const HIDDEN_RETURN_BUFFER: bool = true;
        /// Whether this target's thunks take the caller's hidden return buffer.
        #[cfg(not(windows))]
        pub const HIDDEN_RETURN_BUFFER: bool = false;

        #[cfg(windows)]
        $crate::__khook_thunks_msvc_sret! { [$this_ty $(, $arg: $marker $aty)*] }
        #[cfg(not(windows))]
        $crate::__khook_thunks_plain! { [$this_ty $(, $arg: $marker $aty)*] }
    };
    (no_sret, sret, [$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*]) => {
        /// Whether this target's thunks take the caller's hidden return buffer: Itanium (the
        /// `sret_itanium` flag) does, MSVC (no `sret_msvc`) does not.
        #[cfg(windows)]
        pub const HIDDEN_RETURN_BUFFER: bool = false;
        /// Whether this target's thunks take the caller's hidden return buffer.
        #[cfg(not(windows))]
        pub const HIDDEN_RETURN_BUFFER: bool = true;

        #[cfg(windows)]
        $crate::__khook_thunks_plain! { [$this_ty $(, $arg: $marker $aty)*] }
        #[cfg(not(windows))]
        $crate::__khook_thunks_itanium_sret! { [$this_ty $(, $arg: $marker $aty)*] }
    };
    (sret, sret, [$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*]) => {
        /// Whether this target's thunks take the caller's hidden return buffer: `true` here —
        /// both ABIs return this signature's value through a pointer.
        pub const HIDDEN_RETURN_BUFFER: bool = true;

        #[cfg(windows)]
        $crate::__khook_thunks_msvc_sret! { [$this_ty $(, $arg: $marker $aty)*] }
        #[cfg(not(windows))]
        $crate::__khook_thunks_itanium_sret! { [$this_ty $(, $arg: $marker $aty)*] }
    };
}

/// The thunks for a signature with no hidden return buffer (internal). `Ret` is the module's
/// return type.
#[doc(hidden)]
#[macro_export]
macro_rules! __khook_thunks_plain {
    ([$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*]) => {
        /// The signature KHook's trampoline sees: the hooked function's own, `this` first.
        type ThunkFn = unsafe extern "C" fn($this_ty $(, $aty)*) -> Ret;

        /// `khook.hpp:759-765` `_KHook_Callback_PRE`: run the pre callbacks, fold their
        /// answers and save the result for this call.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn pre(this: $this_ty $(, $arg: $aty)*) -> Ret {
            unsafe {
                // Depth for `in_thunk`/`Hook::remove_sync`: a synchronous removal from inside a
                // thunk would spin on the mutex this call holds shared (detour.cpp:1503).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(ctx) = $crate::hook::ctx_opt::<Ctx>() else {
                    $crate::hook::report_thunk_without_api("pre");
                    return <Ret as Default>::default();
                };
                let mut call = $crate::hook::HookCall::new(this);
                let (action, value) = $crate::hook::dispatch(
                    core::ptr::addr_of!((*ctx).pre),
                    |cbs| {
                        let mut action = $crate::hook::Action::Ignore;
                        let mut value: core::option::Option<Ret> = core::option::Option::None;
                        for cb in cbs.iter() {
                            let r = cb(&mut call, this $(, $arg)*);
                            if r.action > action {
                                action = r.action;
                                value = core::option::Option::Some(r.value);
                            }
                        }
                        (action, value)
                    },
                );
                let ptr: *const Ret = match &value {
                    core::option::Option::Some(v) => core::ptr::from_ref(v),
                    core::option::Option::None => core::ptr::addr_of!((*ctx).fake_return),
                };
                $crate::hook::save_value(action, ptr, false);
                core::ptr::read(core::ptr::addr_of!((*ctx).fake_return))
            }
        }

        /// `khook.hpp:768-774` `_KHook_Callback_POST`: the same, after the original.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn post(this: $this_ty $(, $arg: $aty)*) -> Ret {
            unsafe {
                // Depth for `in_thunk`/`Hook::remove_sync`: a synchronous removal from inside a
                // thunk would spin on the mutex this call holds shared (detour.cpp:1503).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(ctx) = $crate::hook::ctx_opt::<Ctx>() else {
                    $crate::hook::report_thunk_without_api("post");
                    return <Ret as Default>::default();
                };
                let mut call = $crate::hook::HookCall::new(this);
                let (action, value) = $crate::hook::dispatch(
                    core::ptr::addr_of!((*ctx).post),
                    |cbs| {
                        let mut action = $crate::hook::Action::Ignore;
                        let mut value: core::option::Option<Ret> = core::option::Option::None;
                        for cb in cbs.iter() {
                            let r = cb(&mut call, this $(, $arg)*);
                            if r.action > action {
                                action = r.action;
                                value = core::option::Option::Some(r.value);
                            }
                        }
                        (action, value)
                    },
                );
                let ptr: *const Ret = match &value {
                    core::option::Option::Some(v) => core::ptr::from_ref(v),
                    core::option::Option::None => core::ptr::addr_of!((*ctx).fake_return),
                };
                $crate::hook::save_value(action, ptr, false);
                core::ptr::read(core::ptr::addr_of!((*ctx).fake_return))
            }
        }

        /// `khook.hpp:791-801` `_KHook_MakeOriginalCall`: call the unhooked function and save
        /// its result as the original value.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn make_call_original(this: $this_ty $(, $arg: $aty)*) -> Ret {
            unsafe {
                // Depth for `in_thunk`: the call KHook makes the original inside is still this
                // hooked call, and its capsule holds `_detour_mutex` shared (detour.cpp:214).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(original) = $crate::hook::original_function_ptr() else {
                    return <Ret as Default>::default();
                };
                let f: unsafe extern "C" fn($this_ty $(, $aty)*) -> Ret =
                    core::mem::transmute(original);
                let r = f(this $(, $arg)*);
                $crate::hook::save_value(
                    $crate::hook::Action::Ignore,
                    core::ptr::from_ref(&r),
                    true,
                );
                r
            }
        }

        /// `khook.hpp:779-788` `_KHook_MakeReturn`: the value the hooked call returns once
        /// the callbacks had their say.
        ///
        /// With no API installed it answers the type's default value rather than panicking, and
        /// that is a broken process, not a fallback: `DestroyReturnValue` is what pops KHook's
        /// saved-params stack and releases the capsule's `_detour_mutex` shared
        /// (`detour.cpp:408-433`, `khook.hpp:779-788`), so skipping it leaves that capsule locked
        /// shared and the stack pushed on this thread; later synchronous removals on it spin, its
        /// destructor blocks (`detour.cpp:1427`), and the thread's next detour fails the stack
        /// check (`:172-175`). The thunk only avoids crashing.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn make_return(this: $this_ty $(, $arg: $aty)*) -> Ret {
            unsafe {
                let _ = (this $(, $arg)*);
                let Some(api) = $crate::hook::api() else {
                    $crate::hook::report_thunk_without_api("make_return");
                    return <Ret as Default>::default();
                };
                if core::mem::size_of::<Ret>() == 0 {
                    api.destroy_return_value();
                    <Ret as Default>::default()
                } else {
                    let p = api.get_current_value_ptr(true);
                    if p.is_null() {
                        // KHook stores the value before it calls this thunk (`detour.cpp:963-973`);
                        // a null here is a broken KHook. Debug builds trip the assert, release
                        // builds must not abort the game from a thunk: answer the default value.
                        debug_assert!(false, "KHook has no return value saved for this call");
                        api.destroy_return_value();
                        return <Ret as Default>::default();
                    }
                    let r = core::ptr::read(p.cast::<Ret>());
                    api.destroy_return_value();
                    r
                }
            }
        }
    };
}

/// The thunks when MSVC passes the return buffer *after* `this` (internal).
#[doc(hidden)]
#[macro_export]
macro_rules! __khook_thunks_msvc_sret {
    ([$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*]) => {
        /// The signature KHook's trampoline sees: MSVC's hidden return buffer comes right
        /// after `this` and is handed back.
        type ThunkFn = unsafe extern "C" fn($this_ty, *mut Ret $(, $aty)*) -> *mut Ret;

        /// `khook.hpp:759-765` `_KHook_Callback_PRE` for an MSVC member whose struct return
        /// goes through the hidden pointer passed right after `this`.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn pre(
            this: $this_ty,
            ret: *mut Ret
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                // Depth for `in_thunk`/`Hook::remove_sync`: a synchronous removal from inside a
                // thunk would spin on the mutex this call holds shared (detour.cpp:1503).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(ctx) = $crate::hook::ctx_opt::<Ctx>() else {
                    $crate::hook::report_thunk_without_api("pre");
                    return ret;
                };
                let mut call = $crate::hook::HookCall::new(this);
                let (action, value) = $crate::hook::dispatch(
                    core::ptr::addr_of!((*ctx).pre),
                    |cbs| {
                        let mut action = $crate::hook::Action::Ignore;
                        let mut value: core::option::Option<Ret> = core::option::Option::None;
                        for cb in cbs.iter() {
                            let r = cb(&mut call, this $(, $arg)*);
                            if r.action > action {
                                action = r.action;
                                value = core::option::Option::Some(r.value);
                            }
                        }
                        (action, value)
                    },
                );
                let ptr: *const Ret = match &value {
                    core::option::Option::Some(v) => core::ptr::from_ref(v),
                    core::option::Option::None => core::ptr::addr_of!((*ctx).fake_return),
                };
                $crate::hook::save_value(action, ptr, false);
                ret
            }
        }

        /// `khook.hpp:768-774` `_KHook_Callback_POST` for an MSVC member whose struct return
        /// goes through the hidden pointer passed right after `this`.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn post(
            this: $this_ty,
            ret: *mut Ret
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                // Depth for `in_thunk`/`Hook::remove_sync`: a synchronous removal from inside a
                // thunk would spin on the mutex this call holds shared (detour.cpp:1503).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(ctx) = $crate::hook::ctx_opt::<Ctx>() else {
                    $crate::hook::report_thunk_without_api("post");
                    return ret;
                };
                let mut call = $crate::hook::HookCall::new(this);
                let (action, value) = $crate::hook::dispatch(
                    core::ptr::addr_of!((*ctx).post),
                    |cbs| {
                        let mut action = $crate::hook::Action::Ignore;
                        let mut value: core::option::Option<Ret> = core::option::Option::None;
                        for cb in cbs.iter() {
                            let r = cb(&mut call, this $(, $arg)*);
                            if r.action > action {
                                action = r.action;
                                value = core::option::Option::Some(r.value);
                            }
                        }
                        (action, value)
                    },
                );
                let ptr: *const Ret = match &value {
                    core::option::Option::Some(v) => core::ptr::from_ref(v),
                    core::option::Option::None => core::ptr::addr_of!((*ctx).fake_return),
                };
                $crate::hook::save_value(action, ptr, false);
                ret
            }
        }

        /// `khook.hpp:791-801` `_KHook_MakeOriginalCall`: `SaveReturnValue` is given the
        /// caller's buffer, which holds the original's value.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn make_call_original(
            this: $this_ty,
            ret: *mut Ret
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                // Depth for `in_thunk`: the original call still runs inside this hooked call,
                // whose capsule holds `_detour_mutex` shared (detour.cpp:214).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(original) = $crate::hook::original_function_ptr() else {
                    return ret;
                };
                let f: unsafe extern "C" fn(
                    $this_ty,
                    *mut Ret
                    $(, $aty)*
                ) -> *mut Ret =
                    core::mem::transmute(original);
                let out = f(this, ret $(, $arg)*);
                debug_assert!(
                    core::ptr::eq(out.cast_const(), ret.cast_const()),
                    "the hidden return pointer must be the buffer the caller passed"
                );
                $crate::hook::save_value(
                    $crate::hook::Action::Ignore,
                    ret.cast_const(),
                    true,
                );
                out
            }
        }

        /// `khook.hpp:779-788` `_KHook_MakeReturn`: copy the value KHook holds into the
        /// caller's buffer and return it.
        ///
        /// With no API installed it answers the caller's buffer rather than panicking, and that
        /// is a broken process, not a fallback: `DestroyReturnValue` is what pops KHook's
        /// saved-params stack and releases the capsule's `_detour_mutex` shared
        /// (`detour.cpp:408-433`, `khook.hpp:779-788`), so skipping it leaves that capsule locked
        /// shared and the stack pushed on this thread; later synchronous removals on it spin, its
        /// destructor blocks (`detour.cpp:1427`), and the thread's next detour fails the stack
        /// check (`:172-175`). The thunk only avoids crashing.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn make_return(
            this: $this_ty,
            ret: *mut Ret
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                let _ = (this $(, $arg)*);
                let Some(api) = $crate::hook::api() else {
                    $crate::hook::report_thunk_without_api("make_return");
                    return ret;
                };
                if core::mem::size_of::<Ret>() != 0 {
                    let p = api.get_current_value_ptr(true);
                    if p.is_null() {
                        // As in the plain arm: a broken KHook must not abort the game.
                        debug_assert!(false, "KHook has no return value saved for this call");
                        api.destroy_return_value();
                        return ret;
                    }
                    core::ptr::copy_nonoverlapping(p.cast::<Ret>(), ret, 1);
                }
                api.destroy_return_value();
                ret
            }
        }
    };
}

/// The thunks when the Itanium ABI passes the return buffer *before* `this` (internal).
#[doc(hidden)]
#[macro_export]
macro_rules! __khook_thunks_itanium_sret {
    ([$this_ty:ty $(, $arg:ident : $marker:ident $aty:ty)*]) => {
        /// The signature KHook's trampoline sees: the Itanium hidden return buffer comes
        /// before `this` and is handed back.
        type ThunkFn = unsafe extern "C" fn(*mut Ret, $this_ty $(, $aty)*) -> *mut Ret;

        /// `khook.hpp:759-765` `_KHook_Callback_PRE` for an Itanium member whose struct
        /// return goes through the hidden pointer passed before `this`.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn pre(
            ret: *mut Ret,
            this: $this_ty
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                // Depth for `in_thunk`/`Hook::remove_sync`: a synchronous removal from inside a
                // thunk would spin on the mutex this call holds shared (detour.cpp:1503).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(ctx) = $crate::hook::ctx_opt::<Ctx>() else {
                    $crate::hook::report_thunk_without_api("pre");
                    return ret;
                };
                let mut call = $crate::hook::HookCall::new(this);
                let (action, value) = $crate::hook::dispatch(
                    core::ptr::addr_of!((*ctx).pre),
                    |cbs| {
                        let mut action = $crate::hook::Action::Ignore;
                        let mut value: core::option::Option<Ret> = core::option::Option::None;
                        for cb in cbs.iter() {
                            let r = cb(&mut call, this $(, $arg)*);
                            if r.action > action {
                                action = r.action;
                                value = core::option::Option::Some(r.value);
                            }
                        }
                        (action, value)
                    },
                );
                let ptr: *const Ret = match &value {
                    core::option::Option::Some(v) => core::ptr::from_ref(v),
                    core::option::Option::None => core::ptr::addr_of!((*ctx).fake_return),
                };
                $crate::hook::save_value(action, ptr, false);
                ret
            }
        }

        /// `khook.hpp:768-774` `_KHook_Callback_POST` for an Itanium member whose struct
        /// return goes through the hidden pointer passed before `this`.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn post(
            ret: *mut Ret,
            this: $this_ty
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                // Depth for `in_thunk`/`Hook::remove_sync`: a synchronous removal from inside a
                // thunk would spin on the mutex this call holds shared (detour.cpp:1503).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(ctx) = $crate::hook::ctx_opt::<Ctx>() else {
                    $crate::hook::report_thunk_without_api("post");
                    return ret;
                };
                let mut call = $crate::hook::HookCall::new(this);
                let (action, value) = $crate::hook::dispatch(
                    core::ptr::addr_of!((*ctx).post),
                    |cbs| {
                        let mut action = $crate::hook::Action::Ignore;
                        let mut value: core::option::Option<Ret> = core::option::Option::None;
                        for cb in cbs.iter() {
                            let r = cb(&mut call, this $(, $arg)*);
                            if r.action > action {
                                action = r.action;
                                value = core::option::Option::Some(r.value);
                            }
                        }
                        (action, value)
                    },
                );
                let ptr: *const Ret = match &value {
                    core::option::Option::Some(v) => core::ptr::from_ref(v),
                    core::option::Option::None => core::ptr::addr_of!((*ctx).fake_return),
                };
                $crate::hook::save_value(action, ptr, false);
                ret
            }
        }

        /// `khook.hpp:791-801` `_KHook_MakeOriginalCall`: `SaveReturnValue` is given the
        /// caller's buffer, which holds the original's value.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn make_call_original(
            ret: *mut Ret,
            this: $this_ty
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                // Depth for `in_thunk`: the original call still runs inside this hooked call,
                // whose capsule holds `_detour_mutex` shared (detour.cpp:214).
                let _depth = $crate::hook::ThunkGuard::enter();
                let Some(original) = $crate::hook::original_function_ptr() else {
                    return ret;
                };
                let f: unsafe extern "C" fn(
                    *mut Ret,
                    $this_ty
                    $(, $aty)*
                ) -> *mut Ret =
                    core::mem::transmute(original);
                let out = f(ret, this $(, $arg)*);
                debug_assert!(
                    core::ptr::eq(out.cast_const(), ret.cast_const()),
                    "the hidden return pointer must be the buffer the caller passed"
                );
                $crate::hook::save_value(
                    $crate::hook::Action::Ignore,
                    ret.cast_const(),
                    true,
                );
                out
            }
        }

        /// `khook.hpp:779-788` `_KHook_MakeReturn`: copy the value KHook holds into the
        /// caller's buffer and return it.
        ///
        /// With no API installed it answers the caller's buffer rather than panicking, and that
        /// is a broken process, not a fallback: `DestroyReturnValue` is what pops KHook's
        /// saved-params stack and releases the capsule's `_detour_mutex` shared
        /// (`detour.cpp:408-433`, `khook.hpp:779-788`), so skipping it leaves that capsule locked
        /// shared and the stack pushed on this thread; later synchronous removals on it spin, its
        /// destructor blocks (`detour.cpp:1427`), and the thread's next detour fails the stack
        /// check (`:172-175`). The thunk only avoids crashing.
        ///
        /// # Safety
        ///
        /// Called by KHook's trampoline with the hooked function's arguments.
        pub unsafe extern "C" fn make_return(
            ret: *mut Ret,
            this: $this_ty
            $(, $arg: $aty)*
        ) -> *mut Ret {
            unsafe {
                let _ = (this $(, $arg)*);
                let Some(api) = $crate::hook::api() else {
                    $crate::hook::report_thunk_without_api("make_return");
                    return ret;
                };
                if core::mem::size_of::<Ret>() != 0 {
                    let p = api.get_current_value_ptr(true);
                    if p.is_null() {
                        // As in the plain arm: a broken KHook must not abort the game.
                        debug_assert!(false, "KHook has no return value saved for this call");
                        api.destroy_return_value();
                        return ret;
                    }
                    core::ptr::copy_nonoverlapping(p.cast::<Ret>(), ret, 1);
                }
                api.destroy_return_value();
                ret
            }
        }
    };
}

/// The removal function every shape shares (internal): KHook calls it with this thread's
/// context pushed, exactly like the C++ templates' `_KHook_RemovedHook` (`khook.hpp:711-719`,
/// `detour.cpp:1536-1541`).
#[doc(hidden)]
#[macro_export]
macro_rules! __khook_removed_fn {
    () => {
        /// `khook.hpp:711-719` `_KHook_RemovedHook`: release the reference KHook held on the
        /// hook's context, so it is freed once the [`s2sdk::hook::Hook`] dropped it too.
        ///
        /// # Safety
        ///
        /// Called by KHook, which guarantees `GetContextPtr()` is this hook's context.
        pub unsafe extern "C" fn removed(id: $crate::hook::HookId) {
            // SAFETY: KHook calls this with the id it registered the hook under
            // (`khook.hpp:711-719`) and pushes the registering context around the call
            // (`detour.cpp:1536-1541`). Nothing here names the context: a reconciled or
            // already-removed id's context may be freed, and this function may run with no API
            // installed (Metamod re-issues async removals for every id a plugin registered after
            // the plugin's own unload), so the id-first removal path is what keeps a panic out of
            // KHook's `extern "C"` frame.
            unsafe { $crate::hook::release_on_removal_by_id(id) }
        }
    };
}

// ---------------------------------------------------------------------------------------
// The unload-timeout backstop (unit test, private to this module)
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod unload_timeout_backstop {
    use super::*;

    /// An `IKHook` that is never asked anything: the case below fabricates the count instead of
    /// driving a mock through a sequence, because no faithful KHook sequence reaches the
    /// backstop any more (a lost removal is reconciled — see `reconcile_lost`).
    struct SilentApi;

    impl KHookApi for SilentApi {
        unsafe fn setup_hook(
            &self,
            _function: *mut c_void,
            _context: *mut c_void,
            _removed_function: *mut c_void,
            _pre: *mut c_void,
            _post: *mut c_void,
            _make_return: *mut c_void,
            _make_call_original: *mut c_void,
            _stack_size: u32,
            _async: bool,
        ) -> HookId {
            unimplemented!("the backstop test never sets a hook up")
        }

        unsafe fn setup_virtual_hook(
            &self,
            _vtable: *mut *mut c_void,
            _index: i32,
            _context: *mut c_void,
            _removed_function: *mut c_void,
            _pre: *mut c_void,
            _post: *mut c_void,
            _make_return: *mut c_void,
            _make_call_original: *mut c_void,
            _stack_size: u32,
            _async: bool,
        ) -> HookId {
            unimplemented!("the backstop test never sets a hook up")
        }

        unsafe fn remove_hook(
            &self,
            _id: HookId,
            _async: bool,
            _hook_removal_fn: *mut c_void,
            _context: *mut c_void,
        ) {
            unimplemented!("the backstop test has no id to remove")
        }

        fn get_context_ptr(&self) -> *mut c_void {
            unimplemented!("not inside a thunk")
        }

        fn get_original_function(&self) -> *mut c_void {
            unimplemented!("not inside a thunk")
        }

        fn get_original_value_ptr(&self) -> *mut c_void {
            unimplemented!("not inside a thunk")
        }

        fn get_override_value_ptr(&self) -> *mut c_void {
            unimplemented!("not inside a thunk")
        }

        fn get_current_value_ptr(&self, _pop: bool) -> *mut c_void {
            unimplemented!("not inside a thunk")
        }

        fn destroy_return_value(&self) {
            unimplemented!("not inside a thunk")
        }

        fn find_original(&self, _function: *mut c_void) -> *mut c_void {
            unimplemented!("the backstop test asks nothing")
        }

        fn find_original_virtual(&self, _vtable: *mut *mut c_void, _index: i32) -> *mut c_void {
            unimplemented!("the backstop test asks nothing")
        }

        unsafe fn do_recall(
            &self,
            _action: Action,
            _ptr_to_return: *mut c_void,
            _return_size: usize,
            _init_op: *mut c_void,
            _deinit_op: *mut c_void,
        ) -> *mut c_void {
            unimplemented!("the backstop test never recalls")
        }

        unsafe fn save_return_value(
            &self,
            _action: Action,
            _ptr_to_return: *mut c_void,
            _return_size: usize,
            _init_op: *mut c_void,
            _deinit_op: *mut c_void,
            _original: bool,
        ) {
            unimplemented!("not inside a thunk")
        }

        fn lookup_signature(
            &self,
            _start: *mut c_void,
            _size: usize,
            _signature: *const c_char,
        ) -> *mut c_void {
            unimplemented!("the backstop test looks nothing up")
        }

        fn was_original_function_skipped(&self) -> bool {
            unimplemented!("not inside a thunk")
        }
    }

    static API: SilentApi = SilentApi;

    /// `live` counting a context that `tracked` cannot explain is the only case left in which
    /// [`remove_all_blocking`] may report [`Error::UnloadTimeout`]: every other way for a removal
    /// not to run is either impossible (a synchronous `RemoveHook` in real KHook does not return
    /// while the id is still associated, `detour.cpp:1581-1589`) or reconciled
    /// (`reconcile_lost`). The count is fabricated here because reaching it through a mock would
    /// mean a mock that invents KHook behaviour; the assertion is that the backstop still refuses
    /// to claim a safe unload.
    #[test]
    fn unload_timeout_still_reports_a_count_tracked_cannot_explain() {
        static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        install(&API);
        assert_eq!(
            live_context_count(),
            0,
            "the test starts from a clean registry"
        );

        registry().live = 1;
        let deadline = Duration::from_millis(20);
        let started = std::time::Instant::now();
        let err = remove_all_blocking(deadline).expect_err("the count never comes down");
        let waited = started.elapsed();
        assert!(
            waited >= deadline,
            "the wait used the deadline ({waited:?} < {deadline:?})"
        );
        match err {
            Error::UnloadTimeout { remaining } => assert_eq!(remaining, 1),
            other => panic!("unexpected error: {other}"),
        }

        // Restore the process-wide count: nothing else can, and this test is the only writer.
        registry().live = 0;
        uninstall();
    }
}
