//! The KHook *insert-thread* race, modelled faithfully: `associated_hooks`, `insert_hooks`,
//! the capsule's `_callbacks` and its `_detour_mutex` `try_lock`, plus the two states a real
//! `RemoveHook` can find (popped-not-inserted, and the stale re-queue afterwards).
//!
//! `hook_mock.rs` models the removal half of KHook's state machine; this file models the half
//! the lost-removal guard exists for. Every state transition below cites the `detour.cpp` line
//! it mirrors (`K:` throughout) and the mock asserts the contracts those lines imply — in
//! particular that this module passes a null caller-supplied removal function and context to
//! `RemoveHook` (`khook.hpp:2188`), `async = false` from `remove_sync`/`remove_all_blocking` and
//! `async = true` from `Drop` (`khook.hpp:673`).
//!
//! # What is modelled
//!
//! * `globals().associated_hooks`: `id -> DetourCapsule*` under a shared/exclusive lock
//!   (`K:1581-1589,1563-1576,1733-1736`).
//! * `globals().insert_hooks`: a list of `(id, details)` under `insert_hooks_mutex`
//!   (`K:1554-1555`, filled at `K:1747`), drained by the insert thread one entry per pass: pop under the lock, unlock,
//!   `__InsertHook_Sync`, relock, re-queue on failure (`K:1603-1628`).
//! * `DetourCapsule::_callbacks` and `_detour_mutex`: `InsertHook` takes the mutex with
//!   `try_lock` and answers `false` when it cannot (`K:1446-1455`); `RemoveHook` takes it
//!   (retrying until it gets it, which is what `__RemoveHook_Sync`'s 1 ms loop amounts to,
//!   `K:1497-1503,1596`), unlinks `_callbacks[id]` and calls the removal function with the
//!   registering context pushed (`K:1531-1543`). An id that is not in `_callbacks` is answered
//!   with `true` **without** calling the removal function (`K:1543`).
//! * `RemoveHook`'s `insert_hooks` scan (`K:1819-1850`) — it erases the entry, erases the
//!   association and calls the stored removal function, all under `insert_hooks_mutex` — and
//!   the asynchronous queue plus its delete thread (`K:1852-1869,1630-1669`).
//! * `FindOriginal`/`FindOriginalVirtual`: the pre-hook entry recorded at setup, else the
//!   function itself / the slot's current entry (`K:1897-1915`).
//!
//! # What is not modelled
//!
//! * `Shutdown`: the guard's concern is unload *without* it; the wrapper never calls it.
//! * A detached trampoline. The shared hold a hooked call has on its capsule's `_detour_mutex`
//!   (`K:214`) is modelled twice: [`RaceMock`]'s `insert_fails` switch makes the insert path's
//!   `try_lock` fail (`K:1453-1455`), and
//!   `a_hooked_call_in_flight_only_delays_the_removals` holds the modelled capsule's mutex for
//!   the length of a "call". The removal path never fails on it — its real loop sleeps 1 ms and
//!   retries (`K:1596`) — so a test cannot make a *removal* `try_lock` fail and hang.
//!
//! The tests serialize on [`TEST_LOCK`] because the API slot and the registry are process-wide
//! ([`s2sdk::hook::install`]). Each case installs a fresh mock, but **not** a fresh hook-id space:
//! KHook's counter is one global of the library (`K:1546-1548`, `++` at `K:1728-1729`), so ids
//! keep counting across cases ([`NEXT_ID`]) and the wrapper's permanent `lost`/`removal_fired`/
//! `removed_before_registration` records can never match a later case's id.

use core::cell::RefCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use s2sdk::hook::{self, HookCall, HookId, KHookApi, Removal, Return, SetupMode};
use s2sdk::khook_thunks;

// -----------------------------------------------------------------------------------------
// The hooked signature
// -----------------------------------------------------------------------------------------

khook_thunks! {
    /// `void f(this)`.
    pub mod h_void: fn(this: *mut c_void) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

/// A drop probe: a callback owns one, so the counter moves exactly when the hook context is
/// freed.
#[derive(Clone)]
struct Probe(Arc<AtomicUsize>);

impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// The `this` pointer: never dereferenced, only carried through the thunk as KHook does.
fn fake_this() -> *mut c_void {
    static THIS: AtomicUsize = AtomicUsize::new(0x5A5A_5A5A);
    (&THIS as *const AtomicUsize).cast_mut().cast()
}

// -----------------------------------------------------------------------------------------
// The mock
// -----------------------------------------------------------------------------------------

/// `DetourCapsule::InsertHookDetails` (`detour.hpp:60-72`: the context and removal function stored
/// per registration, filled in at `K:1685-1693`): what KHook calls when it removes the hook and what it pushes
/// around that call.
#[derive(Clone, Copy, Debug)]
struct Details {
    context: usize,
    removed: usize,
}

/// One call this module made into KHook, with the arguments it made it with.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    SetupHook {
        function: usize,
        context: usize,
        removed: usize,
        r#async: bool,
    },
    SetupVirtualHook {
        vtable: usize,
        index: i32,
        context: usize,
        removed: usize,
        r#async: bool,
    },
    RemoveHook {
        id: HookId,
        r#async: bool,
        hook_removal_fn: usize,
        context: usize,
    },
    FindOriginal(usize),
    FindOriginalVirtual(usize, i32),
}

/// One `DetourCapsule`: `_callbacks` (`K:1476-1494,1537`) and `_detour_mutex` (`K:1453-1455`).
#[derive(Default)]
struct Capsule {
    /// `_callbacks[id] -> LinkedList` + the details the capsule copied into it (`K:1491-1492`, `detour.hpp:125-134`).
    /// The mutex *is* `_detour_mutex`.
    callbacks: Mutex<BTreeMap<HookId, Details>>,
}

#[derive(Default)]
struct Counters {
    /// How many times KHook ran a removal function for an id (`K:1537-1543,1837-1843`).
    removal_fn: BTreeMap<HookId, usize>,
    /// Synchronous `__RemoveHook_Sync` entries per id (`K:1578-1598`).
    sync_removes: BTreeMap<HookId, usize>,
    /// Asynchronous `RemoveHook` queueing per id (`K:1852-1869`).
    async_removes: BTreeMap<HookId, usize>,
}

impl Counters {
    fn bump(
        map: &Mutex<Counters>,
        f: impl FnOnce(&mut Counters) -> &mut BTreeMap<HookId, usize>,
        id: HookId,
    ) {
        let mut counters = map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = f(&mut counters).entry(id).or_insert(0);
        *entry += 1;
    }

    fn get(
        map: &Mutex<Counters>,
        f: impl Fn(&Counters) -> &BTreeMap<HookId, usize>,
        id: HookId,
    ) -> usize {
        let counters = map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&counters).get(&id).copied().unwrap_or(0)
    }
}

/// One `insert_hooks` entry (`detour.cpp:1555,1607-1610`): the id and the details KHook stored.
type Entry = (HookId, Details);

/// KHook's insert-side state machine, driven by the wrapper and by an "insert thread" the tests
/// run (a real thread in the stress case).
struct RaceMock {
    calls: Mutex<Vec<Call>>,
    /// Off in the stress case: a thousand iterations must not grow a call log, and the
    /// counters below carry the invariants it needs.
    recording: AtomicBool,
    /// `globals().associated_hooks` (`K:1552-1553`, filled at `K:1735`): id -> capsule index.
    associated: std::sync::RwLock<BTreeMap<HookId, usize>>,
    /// `globals().insert_hooks` (`K:1554-1555`, filled at `K:1747`).
    insert_hooks: Mutex<VecDeque<(HookId, Details)>>,
    /// `globals().hooks_detour` (`K:1551`): the `unique_identifier` -> capsule index. A capsule
    /// lives for the process (`K:1696-1719`).
    capsules: Mutex<Vec<Arc<Capsule>>>,
    identifiers: Mutex<BTreeMap<usize, usize>>,
    /// The context each id was registered with, i.e. what `GetContextPtr` hands back while KHook
    /// runs that hook's thunks or its removal function (`K:1686,1531-1541`).
    contexts: Mutex<BTreeMap<HookId, usize>>,
    /// What `FindOriginal`/`FindOriginalVirtual` answer for an identifier: the pre-hook entry
    /// recorded at setup (`K:1897-1915`).
    originals: Mutex<BTreeMap<usize, usize>>,
    /// `globals().delete_hooks` (`K:1556-1557`) plus the delete thread's wake-up
    /// (`K:1630-1669`).
    delete_hooks: Mutex<VecDeque<HookId>>,
    delete_queued: Condvar,
    counters: Mutex<Counters>,
    /// Stands in for a hooked call holding the capsule's `_detour_mutex` shared (`K:214`), so
    /// the insert thread's `try_lock` fails (`K:1453-1455`) and the details are re-queued
    /// (`K:1618-1622`). The removal path is never made to fail on it (see the module docs).
    insert_fails: AtomicBool,
    /// How many `Setup*Hook`/`RemoveHook` calls of this module are inside the mock right now,
    /// and the highest that ever was: the gate test asserts the maximum is 1.
    in_api: AtomicUsize,
    max_in_api: AtomicUsize,
    /// Armed by [`RaceMock::arm_gate_rendezvous`]: the first caller into the API waits
    /// (bounded) for a second one to be inside at the same moment, so the gate test asks two
    /// threads to overlap instead of hoping they do.
    gate_rendezvous_armed: Mutex<bool>,
    gate_rendezvous_changed: Condvar,
    /// Armed by [`RaceMock::arm_scan_hold`]: a synchronous removal whose `insert_hooks` scan
    /// found nothing stops between KHook's answer and this module's next instruction, until the
    /// stale scan on the other thread is inside the context's destructor. That is the one
    /// scheduling point the reconcile race needs (see
    /// `stale_scan_racing_the_reconcile_accounts_for_the_id_once`).
    scan_hold: Mutex<Option<Arc<ScanHold>>>,
}

/// The rendezvous of [`RaceMock::arm_scan_hold`].
struct ScanHold {
    /// The id whose synchronous removal waits here, set when the hold is armed (the id is only
    /// known after the registration it belongs to).
    id: AtomicU32,
    state: Mutex<ScanState>,
    changed: Condvar,
}

/// The flags of a [`ScanHold`], each set by one side of the race.
#[derive(Debug, Default)]
struct ScanState {
    /// The synchronous removal has passed its scan and is waiting for the stale scan.
    scanned: bool,
    /// The stale scan's removal function is inside the context's destructor, i.e. it is past
    /// its own accounting of the id.
    dropping: bool,
    /// The test let the destructor return.
    released: bool,
}

impl ScanHold {
    fn new() -> Self {
        Self {
            id: AtomicU32::new(u32::MAX),
            state: Mutex::new(ScanState::default()),
            changed: Condvar::new(),
        }
    }

    /// The id this hold waits for.
    fn arm_for(&self, id: HookId) {
        self.id.store(id, Ordering::SeqCst);
    }

    /// Is this hold armed for `id` (and not yet fired)?
    fn is_armed_for(&self, id: HookId) -> bool {
        self.id.load(Ordering::SeqCst) == id && !self.scanned()
    }

    /// Set flags and wake everyone waiting for them.
    fn set(&self, f: impl FnOnce(&mut ScanState)) {
        f(&mut self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
        self.changed.notify_all();
    }

    /// Wait until `ready`, `deadline` only as a failure bound. `true` means it became true.
    fn wait(&self, ready: impl Fn(&ScanState) -> bool, deadline: Duration) -> bool {
        let (state, timed_out) = self
            .changed
            .wait_timeout_while(
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                deadline,
                |state| !ready(state),
            )
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !timed_out.timed_out() && ready(&state)
    }

    /// Has this hold fired?
    fn scanned(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .scanned
    }

    /// Wait until the synchronous removal has passed its scan and is waiting for the drop.
    fn wait_for_scanned(&self, deadline: Duration) {
        assert!(
            self.wait(|state| state.scanned, deadline),
            "no synchronous removal reached the scan hold within {deadline:?}"
        );
    }

    /// Wait until the stale scan's removal function is inside the destructor.
    fn wait_for_dropping(&self, deadline: Duration) {
        assert!(
            self.wait(|state| state.dropping, deadline),
            "the stale scan never reached the context's destructor within {deadline:?}"
        );
    }

    /// Let the destructor return (the bounded wait in the probe is the fallback).
    fn release(&self) {
        self.set(|state| state.released = true);
    }

}

impl Default for RaceMock {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            recording: AtomicBool::new(true),
            associated: std::sync::RwLock::new(BTreeMap::new()),
            insert_hooks: Mutex::new(VecDeque::new()),
            capsules: Mutex::new(Vec::new()),
            identifiers: Mutex::new(BTreeMap::new()),
            contexts: Mutex::new(BTreeMap::new()),
            originals: Mutex::new(BTreeMap::new()),
            delete_hooks: Mutex::new(VecDeque::new()),
            delete_queued: Condvar::new(),
            counters: Mutex::new(Counters::default()),
            insert_fails: AtomicBool::new(false),
            in_api: AtomicUsize::new(0),
            max_in_api: AtomicUsize::new(0),
            gate_rendezvous_armed: Mutex::new(false),
            gate_rendezvous_changed: Condvar::new(),
            scan_hold: Mutex::new(None),
        }
    }
}

/// Counts one of this module's `Setup*`/`RemoveHook` calls as inside the mock, and notes the
/// concurrency maximum on drop (so an early return still decrements).
struct ApiEntry<'a>(&'a RaceMock);

impl Drop for ApiEntry<'_> {
    fn drop(&mut self) {
        self.0.in_api.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RaceMock {
    /// Count a `Setup*`/`RemoveHook` call as inside the mock, note the concurrency maximum, and —
    /// when a test armed the rendezvous — wait (bounded) for a second caller, so the gate test
    /// really asks two threads to overlap instead of hoping the scheduler does it.
    fn enter_api(&self) -> ApiEntry<'_> {
        let now = self.in_api.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_api.fetch_max(now, Ordering::SeqCst);
        let mut armed = self
            .gate_rendezvous_armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *armed {
            *armed = false;
            // Bounded: with `KHOOK_GATE` in place the second caller cannot be inside here until
            // this one returns, so the wait runs into this bound exactly once per test.
            let _guard = self
                .gate_rendezvous_changed
                .wait_timeout_while(armed, Duration::from_millis(50), |_| {
                    self.in_api.load(Ordering::SeqCst) < 2
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        self.gate_rendezvous_changed.notify_all();
        ApiEntry(self)
    }

    /// Arm the rendezvous of [`RaceMock::enter_api`] for the next caller.
    fn arm_gate_rendezvous(&self) {
        *self
            .gate_rendezvous_armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    /// Arm the scan hold (`hold`) for a synchronous removal of `id`.
    fn arm_scan_hold(&self, hold: &Arc<ScanHold>, id: HookId) {
        hold.arm_for(id);
        *self
            .scan_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(hold));
    }

    /// The first synchronous removal of the armed id stops here, between its (empty) scan and its
    /// answer, until the stale scan on the other thread is inside the destructor. `false` when no
    /// hold is armed for this id or it already fired.
    fn wait_at_scan_hold(&self, id: HookId) -> bool {
        let hold = {
            let slot = self
                .scan_hold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match slot.as_ref() {
                Some(hold) if hold.is_armed_for(id) => Some(Arc::clone(hold)),
                _ => None,
            }
        };
        let Some(hold) = hold else {
            return false;
        };
        hold.set(|state| state.scanned = true);
        hold.wait_for_dropping(Duration::from_secs(5));
        true
    }

    fn record(&self, call: Call) {
        if self.recording.load(Ordering::SeqCst) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(call);
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn capsule(&self, index: usize) -> Arc<Capsule> {
        Arc::clone(
            &self
                .capsules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[index],
        )
    }

    /// The capsule a hooked address got at setup (`hooks_detour[identifier]`,
    /// `K:1696-1719`), for a test that stands in for a hooked call holding its `_detour_mutex`
    /// shared.
    fn capsule_index(&self, identifier: usize) -> usize {
        self.identifiers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&identifier)
            .copied()
            .expect("the address was registered through this mock")
    }

    /// `hooks_detour[unique_identifier]`, created on the first registration on an address
    /// (`K:1696-1719`). `true` means this call created it, which is what makes KHook insert
    /// synchronously whatever `async` says (`K:1712-1717`).
    fn capsule_for(&self, identifier: usize) -> (usize, bool) {
        let mut identifiers = self
            .identifiers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(&index) = identifiers.get(&identifier) {
            return (index, false);
        }
        let index = {
            let mut capsules = self
                .capsules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            capsules.push(Arc::new(Capsule::default()));
            capsules.len() - 1
        };
        identifiers.insert(identifier, index);
        (index, true)
    }

    /// `__Setup__Hook` (`K:1671-1756`): create/lookup the capsule, allocate an id, associate,
    /// insert synchronously on a fresh capsule or queue the entry for the insert thread.
    fn setup(
        &self,
        identifier: usize,
        original: usize,
        context: *mut c_void,
        removed_function: *mut c_void,
        r#async: bool,
    ) -> HookId {
        let (capsule_index, created) = self.capsule_for(identifier);
        self.originals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(identifier, original);
        // `K:1726-1730`: the id comes from KHook's single global counter, so it is never reused —
        // in this process, across `IKHook` instances and across this file's mocks (`K:1548`,
        // `++` at `K:1729` under `hook_id_mutex`). That is what lets the permanent `lost` set of
        // the wrapper keep ignoring ids from earlier cases.
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst) + 1;
        // `K:1733-1736`: associate before any insert is attempted.
        self.associated
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, capsule_index);
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, context as usize);
        let details = Details {
            context: context as usize,
            removed: removed_function as usize,
        };
        // `K:1712-1717,1738-1748`: a fresh capsule forces `async = false`, so the insert is
        // *asked* for synchronously — and can still fail, because the address was patched before
        // the capsule lock was released (`K:1705,1710`), so a call may be in flight: the capsule's
        // `try_lock` (`K:1453-1455`) then fails and the entry is queued. A later registration
        // asks for a synchronous insert only when the flag says so, with the same fallback.
        // Anything else is queued for the insert thread — an `async = false` registration that
        // cannot take the capsule is queued too, never dropped.
        let mut queued = r#async;
        if created || !r#async {
            // `__InsertHook_Sync`'s capsule `try_lock` (`K:1739` calling `:1563-1576`).
            queued = !self.insert_into_capsule(capsule_index, id, details);
        }
        if queued {
            self.insert_hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back((id, details));
        }
        id
    }

    /// `DetourCapsule::InsertHook` (`K:1446-1494`): `try_lock` the capsule, answer `false` when
    /// it is held (a hooked call, `K:214`), otherwise link `_callbacks[id]` and copy the details.
    fn insert_into_capsule(&self, capsule_index: usize, id: HookId, details: Details) -> bool {
        if self.insert_fails.load(Ordering::SeqCst) {
            return false;
        }
        let capsule = self.capsule(capsule_index);
        let mut callbacks = match capsule.callbacks.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return false,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        callbacks.insert(id, details);
        true
    }

    /// `K:1603-1610`: the insert thread pops the oldest entry under `insert_hooks_mutex` and
    /// releases the lock (`K:1613`) before `__InsertHook_Sync`. Returned to the caller so a test
    /// can hold it in exactly that window.
    fn insert_thread_pop(&self) -> Option<(HookId, Details)> {
        self.insert_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    /// `__InsertHook_Sync` (`K:1563-1576`) plus `DetourCapsule::InsertHook` (`K:1446-1455`).
    /// `true` means done — either inserted, or the id is no longer associated and the details are
    /// dropped; `false` means the caller re-queues them (`K:1618-1622`).
    fn insert_thread_apply(&self, id: HookId, details: &Details) -> bool {
        let associated = self
            .associated
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(&capsule_index) = associated.get(&id) else {
            // `K:1566-1571`: not associated any more, the details are dropped and KHook calls
            // nothing — this is the second half of the lost-removal race.
            return true;
        };
        self.insert_into_capsule(capsule_index, id, *details)
    }

    /// `K:1618-1623`: insert failed, push the same `(id, details)` back.
    fn insert_thread_requeue(&self, id: HookId, details: Details) {
        self.insert_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back((id, details));
    }

    /// `K:1618-1623` for an entry that was older than the popped one: push it back at the head, in
    /// its own order (the test pops until its own id, then puts the foreign entries back).
    fn insert_thread_requeue_front(&self, id: HookId, details: Details) {
        self.insert_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_front((id, details));
    }

    /// `K:1607-1610` until `id` is seen: KHook's insert thread pops the oldest entry, which may be
    /// the stale re-queue of an older lost id, so "the next pop is mine" is not an invariant. The
    /// caller puts `foreign` back at the head in order.
    fn insert_thread_pop_until(&self, id: HookId) -> (Option<Entry>, Vec<Entry>) {
        let mut foreign = Vec::new();
        let mut queue = self
            .insert_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(entry) = queue.pop_front() {
            if entry.0 == id {
                return (Some(entry), foreign);
            }
            foreign.push(entry);
        }
        (None, foreign)
    }

    /// One whole insert-thread pass with nothing racing it: what a healthy queue does.
    fn insert_all_pending(&self) {
        while let Some((id, details)) = self.insert_thread_pop() {
            if !self.insert_thread_apply(id, &details) {
                self.insert_thread_requeue(id, details);
            }
        }
    }

    fn has_queued(&self, id: HookId) -> bool {
        self.insert_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(queued, _)| *queued == id)
    }

    fn queue_len(&self) -> usize {
        self.insert_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn is_associated(&self, id: HookId) -> bool {
        self.associated
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id)
    }

    /// The context KHook pushes around every thunk and removal call of this id
    /// (`K:1686,1531-1541`).
    fn context_of(&self, id: HookId) -> *mut c_void {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .copied()
            .map(|context| context as *mut c_void)
            .expect("the hook was registered through this mock")
    }

    fn capsule_has_callback(&self, id: HookId) -> bool {
        let index = {
            let associated = self
                .associated
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            associated.get(&id).copied()
        };
        index.is_some_and(|index| {
            self.capsule(index)
                .callbacks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&id)
        })
    }

    fn removal_fn_calls(&self, id: HookId) -> usize {
        Counters::get(&self.counters, |c| &c.removal_fn, id)
    }

    fn sync_removes(&self, id: HookId) -> usize {
        Counters::get(&self.counters, |c| &c.sync_removes, id)
    }

    fn async_removes(&self, id: HookId) -> usize {
        Counters::get(&self.counters, |c| &c.async_removes, id)
    }

    /// `__RemoveHook_Sync` (`K:1578-1598`) with `wait` selecting the retry loop over the delete
    /// thread's single `try_lock` (`K:1593-1595`).
    ///
    /// The association's write lock is held across the capsule removal, the removal-function call
    /// and the erase — exactly as `K:1581-1589` holds `associated_hooks_mutex` exclusively around
    /// `DetourCapsule::RemoveHook` and the `associated_hooks.erase` — and released between the
    /// `try_lock` retries, as `K:1593-1597` releases it for the 1 ms pause. That is what makes a
    /// synchronous removal a barrier: no other path (the delete thread included) can be inside
    /// the removal function of this id while this call returns. The 1 ms `sleep_for` is a
    /// `yield_now` here; the loop panics instead of spinning for ever if the capsule is never
    /// released.
    fn remove_sync_path(&self, id: HookId, wait: bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut associated = self
                .associated
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(&capsule_index) = associated.get(&id) else {
                // `K:1582-1585`: not associated, answer `true` and call nothing.
                return true;
            };
            let capsule = self.capsule(capsule_index);
            // `DetourCapsule::RemoveHook`'s `try_lock` (`K:1503-1506`).
            let mut callbacks = match capsule.callbacks.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    drop(associated);
                    if !wait {
                        // `K:1594-1596`: the delete thread retries this id on a later pass.
                        return false;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the capsule of hook {id} was never released to a synchronous removal"
                    );
                    // `K:1596-1598`: release the association, wait a little, retry.
                    std::thread::yield_now();
                    continue;
                }
            };
            // `K:1509-1543`: unlink `_callbacks[id]`; when it is there, call the stored removal
            // function with the registering context pushed. When it is not, `K:1543` answers
            // `true` without calling anything — the lost-removal path.
            let details = callbacks.remove(&id);
            drop(callbacks);
            if let Some(details) = details {
                // Still under the association's write lock, as `K:1587` calls the capsule's
                // `RemoveHook` (and with it the removal function) inside the
                // `associated_hooks_mutex` guard.
                self.run_removal(id, details);
            }
            // `K:1587-1589`: `RemoveHook` returned true, so `__RemoveHook_Sync` erases the
            // association — under the same guard.
            associated.remove(&id);
            return true;
        }
    }

    /// What KHook does when it has decided to remove a hook: push that hook's context and call
    /// the removal function stored at setup (`K:1531-1543,1837-1843`).
    fn run_removal(&self, id: HookId, details: Details) {
        Counters::bump(&self.counters, |c| &mut c.removal_fn, id);
        let removed: unsafe extern "C" fn(HookId) =
            unsafe { core::mem::transmute(details.removed) };
        THREAD.with(|t| t.borrow_mut().contexts.push(details.context as *mut c_void));
        // SAFETY: the wrapper registered this thunk with exactly this signature
        // (`khook.hpp:711-719`) and the mock stands in for the KHook that calls it.
        unsafe { removed(id) };
        THREAD.with(|t| {
            t.borrow_mut().contexts.pop();
        });
    }

    /// One delete-thread pass: pop the oldest queued removal and run `__RemoveHook_Sync(id,
    /// false)` (`K:1630-1669`); on `false` the id is retried later.
    fn delete_thread_pass(&self) {
        let id = {
            let mut queue = self
                .delete_hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                !queue.is_empty(),
                "the delete thread was woken with nothing to do"
            );
            queue.pop_front().expect("checked above")
        };
        if !self.remove_sync_path(id, false) {
            self.delete_hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(id);
        }
    }

    /// Wait for work and run one delete-thread pass; `deadline` is only the failure bound.
    fn fire_deferred_removal(&self, deadline: Duration) {
        let mut queue = self
            .delete_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, timed_out) = self
            .delete_queued
            .wait_timeout_while(queue, deadline, |q| q.is_empty())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue = guard;
        assert!(
            !timed_out.timed_out() && !queue.is_empty(),
            "no removal reached the mock's delete thread within {deadline:?}"
        );
        drop(queue);
        self.delete_thread_pass();
    }

    fn deferred_len(&self) -> usize {
        self.delete_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn enter(&self, context: *mut c_void) {
        THREAD.with(|t| t.borrow_mut().contexts.push(context));
    }

    fn leave(&self) {
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            assert!(
                t.contexts.pop().is_some(),
                "leave() without a matching enter()"
            );
        });
    }
}

/// KHook's per-thread context stack (`g_current_hook`, `detour.cpp:359-361`).
#[derive(Default)]
struct Thread {
    contexts: Vec<*mut c_void>,
}

thread_local! {
    static THREAD: RefCell<Thread> = RefCell::new(Thread::default());
}

impl KHookApi for RaceMock {
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
    ) -> HookId {
        let _entry = self.enter_api();
        self.record(Call::SetupHook {
            function: function as usize,
            context: context as usize,
            removed: removed_function as usize,
            r#async,
        });
        // The wrapper always hands KHook the five entry points (`detour.hpp:60-72` and `K:1685-1693` fill them in); a
        // null one would mean the removal function this file is about never runs.
        assert!(
            !removed_function.is_null() && !pre.is_null() && !post.is_null(),
            "SetupHook without the generated thunks"
        );
        assert!(!make_return.is_null() && !make_call_original.is_null());
        assert!(!context.is_null(), "SetupHook without a context");
        assert!(stack_size > 0, "the wrapper computes the stack size");
        // KHook identifies an inline hook by the function's own address (`K:1770-1781`).
        self.setup(
            function as usize,
            function as usize,
            context,
            removed_function,
            r#async,
        )
    }

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
    ) -> HookId {
        let _entry = self.enter_api();
        self.record(Call::SetupVirtualHook {
            vtable: vtable as usize,
            index,
            context: context as usize,
            removed: removed_function as usize,
            r#async,
        });
        assert!(
            !removed_function.is_null() && !pre.is_null() && !post.is_null(),
            "SetupVirtualHook without the generated thunks"
        );
        assert!(!make_return.is_null() && !make_call_original.is_null());
        assert!(stack_size > 0);
        // `K:1797`: a virtual hook is identified by the slot's own address, and the
        // original entry is what the slot holds now.
        let slot = vtable.wrapping_add(index as usize);
        // SAFETY: the trait contract is that `vtable` names a live slot of at least `index + 1`
        // entries; reading it is what KHook itself does at setup (`detour.hpp:96-104`).
        let original = unsafe { *slot } as usize;
        self.setup(slot as usize, original, context, removed_function, r#async)
    }

    unsafe fn remove_hook(
        &self,
        id: HookId,
        r#async: bool,
        hook_removal_fn: *mut c_void,
        context: *mut c_void,
    ) {
        let _entry = self.enter_api();
        self.record(Call::RemoveHook {
            id,
            r#async,
            hook_removal_fn: hook_removal_fn as usize,
            context: context as usize,
        });
        // The header passes both, but this module never uses them: KHook calls the removal
        // function it stored at setup either way (`khook.hpp:2188`, `detour.cpp:1812-1877`), and
        // the C++ templates pass null here too. A non-null value would mean this module started
        // relying on a callback whose lifetime KHook owns.
        assert!(
            hook_removal_fn.is_null() && context.is_null(),
            "this module must pass a null caller-supplied removal function and context"
        );
        // `K:1819-1850`: the insert_hooks scan runs first, for both flags, and erases the entry,
        // the association and calls the stored removal function while holding
        // `insert_hooks_mutex`.
        {
            let mut queue = self
                .insert_hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(position) = queue.iter().position(|(queued, _)| *queued == id) {
                let (_, details) = queue.remove(position).expect("found above");
                // `K:1831-1835`: disassociate from the detour.
                self.associated
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                // `K:1837-1843`: call the removal function with the stored context. The queue
                // lock is held across it, exactly as `insert_hooks_mutex` is.
                self.run_removal(id, details);
                return;
            }
        }
        if r#async {
            // `K:1852-1869`: if KHook no longer associates the id it calls only the (null)
            // caller callback and returns; otherwise the removal is queued for the delete thread.
            if !self.is_associated(id) {
                return;
            }
            Counters::bump(&self.counters, |c| &mut c.async_removes, id);
            self.delete_hooks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(id);
            self.delete_queued.notify_all();
            return;
        }
        // The reconcile-race test's scheduling point: this call's scan found nothing (the entry
        // is not queued at this instant) and the test's stale scan on another thread is about to
        // run this id's removal function; stall here until it is inside the destructor, i.e.
        // past its own accounting of the id.
        self.wait_at_scan_hold(id);
        // `K:1870-1876`: a synchronous removal waits for `__RemoveHook_Sync`.
        Counters::bump(&self.counters, |c| &mut c.sync_removes, id);
        self.remove_sync_path(id, true);
    }

    fn get_context_ptr(&self) -> *mut c_void {
        THREAD.with(|t| {
            t.borrow()
                .contexts
                .last()
                .copied()
                .unwrap_or(core::ptr::null_mut())
        })
    }

    fn get_original_function(&self) -> *mut c_void {
        unreachable!("the tests never run make_call_original")
    }

    fn get_original_value_ptr(&self) -> *mut c_void {
        unreachable!("the tests never run a return-value thunk")
    }

    fn get_override_value_ptr(&self) -> *mut c_void {
        unreachable!("the tests never run a return-value thunk")
    }

    fn get_current_value_ptr(&self, _pop: bool) -> *mut c_void {
        unreachable!("the tests never run a return-value thunk")
    }

    fn destroy_return_value(&self) {
        unreachable!("the tests never run a return-value thunk")
    }

    fn find_original(&self, function: *mut c_void) -> *mut c_void {
        self.record(Call::FindOriginal(function as usize));
        // `K:1897-1906`: a hit answers the capsule's original entry; a miss answers the
        // argument ("no associated detours, so this is already original function").
        self.originals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(function as usize))
            .copied()
            .map_or(function, |original| original as *mut c_void)
    }

    fn find_original_virtual(&self, vtable: *mut *mut c_void, index: i32) -> *mut c_void {
        self.record(Call::FindOriginalVirtual(vtable as usize, index));
        let slot = vtable.wrapping_add(index as usize);
        let original = self
            .originals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(slot as usize))
            .copied();
        match original {
            // `K:1907-1915`: the slot is hooked, answer the pre-hook entry.
            Some(original) => original as *mut c_void,
            // SAFETY: the trait contract is a live vtable of at least `index + 1` entries;
            // `K:1914` returns `vtable[index]` exactly like this for an unhooked slot.
            None => unsafe { *slot },
        }
    }

    unsafe fn do_recall(
        &self,
        _action: hook::Action,
        _ptr_to_return: *mut c_void,
        _return_size: usize,
        _init_op: *mut c_void,
        _deinit_op: *mut c_void,
    ) -> *mut c_void {
        unreachable!("the wrapper never recalls a hook")
    }

    unsafe fn save_return_value(
        &self,
        _action: hook::Action,
        ptr_to_return: *mut c_void,
        return_size: usize,
        _init_op: *mut c_void,
        _deinit_op: *mut c_void,
        _original: bool,
    ) {
        // The thunk-depth test runs a real `pre` thunk; with a `void` signature the size is 0
        // and KHook skips the copy (`detour.cpp:373-380`).
        assert!(
            return_size == 0 || !ptr_to_return.is_null(),
            "SaveReturnValue with a size and a null pointer"
        );
    }

    fn lookup_signature(
        &self,
        _start: *mut c_void,
        _size: usize,
        _signature: *const core::ffi::c_char,
    ) -> *mut c_void {
        unreachable!("the wrapper never looks a signature up by itself")
    }

    fn was_original_function_skipped(&self) -> bool {
        unreachable!("the tests never run a return-value thunk")
    }
}

// -----------------------------------------------------------------------------------------
// Diagnostics sink (process-wide, like the one the bridge installs)
// -----------------------------------------------------------------------------------------

static SINK_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn sink(line: &str) {
    SINK_LINES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(line.to_owned());
}

fn take_sink_lines() -> Vec<String> {
    core::mem::take(
        &mut *SINK_LINES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

// -----------------------------------------------------------------------------------------
// Harness
// -----------------------------------------------------------------------------------------

/// Serializes the cases: the API slot and the registry are process-wide.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// KHook's hook-id counter: one for the whole library process (`K:1546-1548`, `++` at `K:1729`
/// under `hook_id_mutex`). It lives here rather than in one mock, exactly because a new `IKHook`
/// does not start a new id space and the wrapper's `lost` set is permanent.
static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// Run one case against a fresh mock. A new mock is a new `IKHook` instance, but **not** a new
/// hook-id space: KHook's counter belongs to the library (`K:1546-1548,1728-1729`), so the ids
/// keep counting across cases (see [`NEXT_ID`]) and the wrapper's permanent `lost` set can never
/// mistake a later case's id for an earlier case's. After the case everything KHook queued for
/// its delete thread is run, the registry is drained, and the case must end with nothing of this
/// module left for KHook to call into.
fn with_mock<R>(body: impl FnOnce(&'static RaceMock) -> R) -> R {
    let guard: MutexGuard<'_, ()> = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        hook::live_context_count(),
        0,
        "a previous case left hook contexts KHook can still call into"
    );
    take_sink_lines();
    hook::set_diagnostic_sink(sink);
    let mock: &'static RaceMock = Box::leak(Box::new(RaceMock::default()));
    hook::install(mock);
    let out = body(mock);
    while mock.deferred_len() > 0 {
        mock.fire_deferred_removal(Duration::from_secs(10));
    }
    let drained = hook::remove_all_blocking(Duration::from_secs(10));
    assert!(
        drained.is_ok(),
        "the case's hooks were still KHook's problem at the end: {drained:?}"
    );
    assert_eq!(
        hook::live_context_count(),
        0,
        "every removal function ran or was reconciled by the end of the case"
    );
    hook::uninstall();
    drop(guard);
    out
}

/// Register an inline hook with an optional drop probe, leaving the entry in KHook's
/// `insert_hooks` (the wrapper asks for `async = true`).
fn register(
    mock: &RaceMock,
    function: *mut c_void,
    probe: Option<Arc<AtomicUsize>>,
) -> s2sdk::hook::Hook {
    let pre: Vec<h_void::PreCb> = match probe {
        None => Vec::new(),
        Some(drops) => vec![Arc::new({
            let probe = Probe(drops);
            move |_call: &mut HookCall, _this: *mut c_void| {
                let _ = &probe;
                Return::ignore(())
            }
        })],
    };
    let _ = mock;
    // SAFETY: `function` is an `extern "C" fn(*mut c_void)` in this test.
    unsafe { h_void::setup_inline(function, pre, Vec::new(), None, SetupMode::Async) }
        .expect("the mock accepts the registration")
}

/// Get an id **queued** in `insert_hooks` with its capsule already created — state (a)'s
/// starting point. The first registration on an address creates the capsule and is *forced*
/// synchronous (`async = false`, `K:1712-1717`); it succeeds here because nothing is in flight on
/// the fresh address, so it can be removed again for good (the capsule itself lives for the
/// process, `K:1696-1719`). The second registration on the same address is a later one on an
/// existing capsule and is queued for the insert thread (`K:1738-1748`). The returned `Hook` is
/// the queued one.
fn queue_later_registration(
    mock: &RaceMock,
    function: *mut c_void,
    probe: Option<Arc<AtomicUsize>>,
) -> (s2sdk::hook::Hook, Details) {
    let first = register(mock, function, None);
    assert_eq!(
        mock.queue_len(),
        0,
        "a fresh capsule is inserted synchronously when nothing is in flight (K:1712-1717)"
    );
    assert_eq!(
        first.remove_sync().expect("removed"),
        Removal::Removed,
        "the first registration is removed; its capsule stays"
    );
    assert_eq!(hook::live_context_count(), 0);
    let hook = register(mock, function, probe);
    let (id, details) = mock
        .insert_thread_pop()
        .expect("a later registration on the existing capsule is queued for the insert thread");
    assert_eq!(hook.id(), id);
    assert!(mock.is_associated(id));
    assert!(!mock.capsule_has_callback(id));
    assert_eq!(mock.queue_len(), 0, "the insert thread is holding it");
    (hook, details)
}

/// Wait until `ready` holds, `deadline` only as a failure bound (a counter polled with
/// `yield_now`, never a bare sleep).
fn wait_until(deadline: Duration, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < deadline,
            "the condition never became true within {deadline:?}"
        );
        std::thread::yield_now();
    }
}

// -----------------------------------------------------------------------------------------
// 1. remove_sync: the normal path
// -----------------------------------------------------------------------------------------

/// A hook KHook still holds in its capsule is removed by `remove_sync` inside the call: the
/// removal function runs once (`K:1537-1543`), KHook's reference is released, the `Hook`
/// releases its own, and the context is freed exactly once. The request is synchronous and
/// passes null caller slots (`khook.hpp:2188`).
#[test]
fn remove_sync_normal_runs_removal_and_frees() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let hook = register(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();
        // The first registration on an address creates the capsule and is forced synchronous
        // (`K:1712-1717`), and nothing is in flight on this fresh address, so the insert lands
        // inside the call and the capsule holds the callback (`K:1458-1494`).
        assert_eq!(mock.queue_len(), 0);
        assert!(mock.capsule_has_callback(id) && mock.is_associated(id));
        assert_eq!(hook::live_context_count(), 1);

        let out = hook.remove_sync().expect("the mock removes it");
        assert_eq!(out, Removal::Removed);
        assert_eq!(mock.removal_fn_calls(id), 1, "the removal function ran once");
        assert_eq!(mock.sync_removes(id), 1, "one synchronous RemoveHook");
        assert_eq!(mock.async_removes(id), 0, "remove_sync never queues");
        assert!(!mock.is_associated(id), "KHook erased the association");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the context was freed once"
        );
        assert_eq!(hook::live_context_count(), 0);

        match mock
            .calls()
            .iter()
            .find(|call| matches!(call, Call::RemoveHook { .. }))
            .expect("a RemoveHook call")
        {
            Call::RemoveHook {
                id: id_of_the_hook,
                r#async,
                hook_removal_fn,
                context,
            } => {
                assert_eq!(*id_of_the_hook, id);
                assert!(!*r#async, "remove_sync asks for the synchronous removal");
                assert_eq!((*hook_removal_fn, *context), (0, 0), "null caller slots");
            }
            other => panic!("unexpected call {other:?}"),
        }
    });
}

/// The other end: a hook KHook already forgot (an earlier `remove_all_blocking`) is answered
/// `AlreadyGone`, and the `Hook`'s own reference frees the context.
#[test]
fn remove_sync_after_a_drain_reports_already_gone() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let hook = register(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();
        mock.insert_all_pending();
        hook::remove_all_blocking(Duration::from_secs(5)).expect("nothing lost here");
        assert_eq!(mock.removal_fn_calls(id), 1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the Hook still holds a reference"
        );

        let removal = hook.remove_sync().expect("KHook has nothing left to do");
        assert_eq!(removal, Removal::AlreadyGone);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the Hook's reference freed it"
        );
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(mock.sync_removes(id), 1, "no second RemoveHook was needed");
    });
}

/// `Drop` is the asynchronous default the C++ templates choose (`khook.hpp:673`): the removal
/// is only queued (`K:1852-1869`) and the delete thread runs it (`K:1630-1669`). The mock
/// records the flag, and the context stays alive until the removal function runs.
#[test]
fn drop_asks_for_the_asynchronous_removal() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let hook = register(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();
        assert!(mock.capsule_has_callback(id));
        drop(hook);
        match mock
            .calls()
            .iter()
            .find(|call| matches!(call, Call::RemoveHook { .. }))
            .expect("Drop asks for a removal")
        {
            Call::RemoveHook {
                id: removed,
                r#async,
                hook_removal_fn,
                context,
            } => {
                assert_eq!(*removed, id);
                assert!(
                    *r#async,
                    "Drop queues the asynchronous removal (khook.hpp:673)"
                );
                assert_eq!((*hook_removal_fn, *context), (0, 0), "null caller slots");
            }
            other => panic!("unexpected call {other:?}"),
        }
        assert_eq!(mock.deferred_len(), 1, "only queued so far");
        assert_eq!(hook::live_context_count(), 1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "KHook still holds the context"
        );
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(mock.removal_fn_calls(id), 1);
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the last reference freed it"
        );
    });
}

/// A hook an earlier drain reconciled is reported as `ReconciledLost` by a later `remove_sync`,
/// not as `AlreadyGone`: the context was leaked on purpose and the `Hook`'s reference must not
/// free it either.
#[test]
fn remove_sync_after_a_reconcile_reports_lost() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let (hook, _held) =
            queue_later_registration(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        hook::remove_all_blocking(Duration::from_secs(5)).expect("reconciled");
        let out = hook.remove_sync().expect("answered");
        assert_eq!(out, Removal::ReconciledLost);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the reconcile's leak is kept"
        );
        assert_eq!(hook::live_context_count(), 0);
    });
}

// -----------------------------------------------------------------------------------------
// 2. The lost removal: state (a), the stale re-queue (b), reconcile and drain
// -----------------------------------------------------------------------------------------

/// State (a): the insert thread popped `(id, details)` and released `insert_hooks_mutex`
/// (`K:1607-1613`) but has not taken `associated_hooks_mutex` yet. The id is associated, absent
/// from `insert_hooks` and absent from the capsule. A synchronous removal finds the id
/// associated, finds no callback to unlink, answers "gone" **without** calling the removal
/// function (`K:1497-1544,1543` not-found path, `K:1587-1589`) and erases the association. The
/// insert thread then finds the id unassociated and drops the details (`K:1566-1571`).
///
/// `remove_sync` must reconcile it: `ReconciledLost`, the count drops, the diagnostic names the
/// id, and the context is **not** freed (a stale entry could still name it).
#[test]
fn lost_removal_is_reconciled() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let (hook, held) =
            queue_later_registration(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();
        assert!(mock.is_associated(id), "associated at setup (K:1733-1736)");
        let lost_before = hook::lost_removal_count();

        let out = hook
            .remove_sync()
            .expect("the removal is answered, not failed");
        assert_eq!(out, Removal::ReconciledLost);
        assert_eq!(
            mock.removal_fn_calls(id),
            0,
            "KHook never ran our removal function"
        );
        assert!(
            !mock.is_associated(id),
            "the capsule answered gone and the id was erased"
        );
        assert_eq!(hook::lost_removal_count(), lost_before + 1);
        assert_eq!(
            hook::live_context_count(),
            0,
            "the reconciled id is not waited for"
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the context is leaked on purpose"
        );

        let lines = take_sink_lines();
        assert!(
            lines.contains(&format!(
                "khook: lost removal for id {id} reconciled (KHook insert race)"
            )),
            "the reconcile line went through the sink: {lines:?}"
        );

        // The insert thread's own step: it now finds the id unassociated and drops the details
        // (`K:1566-1571`), which must not call anything.
        assert!(
            mock.insert_thread_apply(id, &held),
            "not associated any more, so the insert answers done"
        );
        assert_eq!(
            mock.queue_len(),
            0,
            "the details were dropped, not re-queued"
        );
        assert_eq!(mock.removal_fn_calls(id), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// State (b): the stale re-queue. The insert thread's capsule `try_lock` failed (a call in
/// flight holds `_detour_mutex` shared, `K:214,1448-1451`), so `__InsertHook_Sync` answered
/// false and the insert thread pushes the same `(id, details)` back (`K:1618-1622`). If the
/// association is erased before that re-queue happens, a stale entry carrying our removal
/// function and context sits in `insert_hooks` for up to one insert-thread pass, and any later
/// `RemoveHook(id)` scan calls that function (`K:1819-1850`).
///
/// That call must be **ignored**: after the reconcile the `Hook`'s drop released the creator's
/// reference, so the leaked context has exactly KHook's one reference left, and running the
/// removal function would release it and free the context (the drop counter below) — the double
/// release the lost check prevents. Nothing may be freed and `live` must not move.
#[test]
fn stale_requeue_removal_call_is_ignored() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let (hook, held) =
            queue_later_registration(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();

        // State (a): the removal runs while the insert thread is in its window, and reconciles.
        hook::remove_all_blocking(Duration::from_secs(5)).expect("reconciled");
        assert_eq!(mock.removal_fn_calls(id), 0);
        // The Hook goes away normally afterwards: `Drop` releases the creator's reference, so
        // only KHook's (never to be released) is left on the leaked context.
        drop(hook);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // State (b): the insert thread finally re-locks `insert_hooks_mutex` and re-queues the
        // details its failed `__InsertHook_Sync` handed back.
        mock.insert_thread_requeue(id, held);
        assert!(mock.has_queued(id), "the stale entry is in insert_hooks");

        // A drain finds it: the scan erases it and calls our removal function, which has to
        // ignore the reconciled id before it touches the context.
        let lost_before = hook::lost_removal_count();
        hook::remove_all_blocking(Duration::from_secs(5)).expect("the drain is Ok");
        assert_eq!(
            mock.removal_fn_calls(id),
            1,
            "the stale entry's removal function was called once"
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the ignored call must not free the leaked context"
        );
        assert_eq!(
            hook::live_context_count(),
            0,
            "and must not lower the count again"
        );
        assert_eq!(
            hook::lost_removal_count(),
            lost_before,
            "no second reconcile"
        );
        assert!(!mock.has_queued(id), "the drain erased the stale entry");
        assert_eq!(mock.queue_len(), 0);

        let lines = take_sink_lines();
        assert!(
            lines.contains(&"khook: drained 1 lost id(s)".to_owned()),
            "the drain reported what it did: {lines:?}"
        );
    });
}

/// The reconcile inside `remove_all_blocking`: a hook still `installed` (its `Hook` alive) whose
/// insert thread held it is no longer a `UnloadTimeout` — the synchronous re-issue returns
/// without calling the removal function, the id is reconciled, and the call returns `Ok` far
/// below its deadline.
#[test]
fn remove_all_blocking_reconciles_lost_without_timeout() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let (hook, _held) =
            queue_later_registration(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();
        let lost_before = hook::lost_removal_count();

        let started = Instant::now();
        let out = hook::remove_all_blocking(Duration::from_secs(5));
        let waited = started.elapsed();
        assert!(out.is_ok(), "the lost removal is reconciled: {out:?}");
        assert!(
            waited < Duration::from_secs(1),
            "no deadline was waited out ({waited:?})"
        );
        assert_eq!(hook::lost_removal_count(), lost_before + 1);
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(mock.removal_fn_calls(id), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0, "leaked on purpose");

        // The `Hook` is still alive; dropping it now is harmless (the id is not associated) and
        // releases the creator's reference only.
        drop(hook);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    });
}

/// The drain issues exactly one extra synchronous removal per lost id (on top of the loop's
/// re-issue), which is what erases a stale insert entry: the mock counts both per id.
#[test]
fn remove_all_blocking_drains_lost_ids_once() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let (hook, _held) = queue_later_registration(mock, hooked as *mut c_void, None);
        let id = hook.id();

        hook::remove_all_blocking(Duration::from_secs(5)).expect("reconciled");
        assert_eq!(
            mock.sync_removes(id),
            2,
            "the loop's re-issue plus one drain call"
        );
        assert_eq!(
            mock.removal_fn_calls(id),
            0,
            "the queue is empty, so nothing ran"
        );

        // A later drain re-issues each lost id once more: the entry may have been re-queued in
        // between, and only the scan can tell.
        hook::remove_all_blocking(Duration::from_secs(5)).expect("still Ok");
        assert_eq!(mock.sync_removes(id), 3, "one drain call per invocation");
        drop(hook);
    });
}

/// A removal `Drop` queued can be lost by the delete thread itself: the insert thread holds the
/// `(id, details)` in its pop window, so `__RemoveHook_Sync(id, false)` finds the id associated,
/// finds no callback in the capsule (`K:1543`) and erases the association without calling our
/// removal function; the delete thread sees "true" and drops the entry (`K:1644`, `:1637-1656`).
/// Nothing lowers the count and nothing will ever run the removal function on its own — the id
/// stays pending, tracked and counted until `remove_all_blocking` re-issues it synchronously and
/// reconciles it.
#[test]
fn remove_all_blocking_reconciles_a_removal_the_delete_thread_lost() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let (hook, held) =
            queue_later_registration(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();

        // `Drop` queues the asynchronous removal (`K:1852-1869`).
        drop(hook);
        assert_eq!(mock.deferred_len(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // The delete thread's pass: the id is associated but the capsule has no callback for it,
        // so the synchronous removal answers "gone" and calls nothing (`K:1644` calling
        // `:1582-1589` and the capsule's not-found answer `:1543`).
        mock.delete_thread_pass();
        assert!(
            !mock.is_associated(id),
            "the capsule answered gone and the id was erased"
        );
        assert_eq!(mock.removal_fn_calls(id), 0, "KHook never ran our removal function");
        assert_eq!(mock.deferred_len(), 0, "the delete thread dropped the entry");
        assert_eq!(
            hook::live_context_count(),
            1,
            "nothing lowered the count: the removal was lost"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let lost_before = hook::lost_removal_count();

        // Only a synchronous re-issue can find out that KHook no longer holds it.
        let started = Instant::now();
        let out = hook::remove_all_blocking(Duration::from_secs(5));
        let waited = started.elapsed();
        assert!(out.is_ok(), "the lost removal is reconciled: {out:?}");
        assert!(
            waited < Duration::from_secs(1),
            "no deadline was waited out ({waited:?})"
        );
        assert_eq!(hook::lost_removal_count(), lost_before + 1);
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(mock.removal_fn_calls(id), 0);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the context is leaked on purpose"
        );

        // The insert thread's details can still come back as a stale entry (state (b)); the next
        // drain erases it and calls the removal function, which the lost check ignores.
        mock.insert_thread_requeue(id, held);
        assert!(mock.has_queued(id), "the stale entry is back in insert_hooks");
        hook::remove_all_blocking(Duration::from_secs(5)).expect("the drain is Ok");
        assert!(!mock.has_queued(id), "the drain erased the stale entry");
        assert_eq!(mock.removal_fn_calls(id), 1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "the ignored call freed nothing"
        );
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// The removal function and the reconcile must never account for the same id: exactly one of
/// them lowers the count, frees the context or leaks it. This is the race that can make both run
/// for one id — a **stale scan** on another thread (Metamod's per-plugin unloader calling
/// `RemoveHook(id)` outside this module's gate) running our removal function while this thread
/// reconciles the id its own synchronous call found still tracked.
///
/// The mock's scan hold makes it deterministic: the synchronous call stops after its empty
/// `insert_hooks` scan, the stale scan queues the details and removes them (calling our removal
/// function, which frees the context — the `Hook` is gone, so this is KHook's reference only),
/// and only then does the synchronous call return and reconcile. The destructor of the context's
/// callback holds the stale scan inside the window until the reconcile has run, so the two really
/// overlap.
///
/// With the lost check and the claim on `tracked` in one critical section, the stale scan owns
/// the id (`drops == 1`, no reconcile); with them in separate scopes the reconcile fires on top of
/// a removal that already ran (`lost_removal_count()` moves as well), which is the failure this
/// asserts against.
#[test]
fn stale_scan_racing_the_reconcile_accounts_for_the_id_once() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        // The first registration on an address creates the capsule and is inserted
        // synchronously (`K:1712-1717`); removing it leaves the capsule in place, so the
        // registration below is a later one and is queued for the insert thread.
        let warm_up = register(mock, hooked as *mut c_void, None);
        assert_eq!(warm_up.remove_sync().expect("removed"), Removal::Removed);
        let hold = Arc::new(ScanHold::new());
        // The context's callback owns the probe, and its destructor is what wedges the stale
        // scan inside its window.
        let pre: Vec<h_void::PreCb> = vec![Arc::new({
            let probe = RacingProbe {
                drops: Arc::clone(&drops),
                hold: Arc::clone(&hold),
            };
            move |_call: &mut HookCall, _this: *mut c_void| {
                // The probe lives in this callback's environment, which the context owns: it is
                // dropped exactly when the context is.
                let _ = &probe;
                Return::ignore(())
            }
        })];
        let hook = unsafe {
            h_void::setup_inline(hooked as *mut c_void, pre, Vec::new(), None, SetupMode::Async)
        }
        .expect("registered");
        let id = hook.id();
        mock.arm_scan_hold(&hold, id);
        // Put the id in state (a): the insert thread holds its details, the capsule has no
        // callback, and the id is associated.
        let held = mock
            .insert_thread_pop()
            .expect("the later registration is queued for the insert thread");
        assert_eq!(held.0, id);
        assert!(mock.is_associated(id));

        // The `Hook` goes away: its reference is released, so the removal function's release is
        // the last reference and its destructor is where the race can be held open.
        drop(hook);
        assert_eq!(mock.deferred_len(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let lost_before = hook::lost_removal_count();

        std::thread::scope(|scope| {
            let stale = {
                let hold = Arc::clone(&hold);
                scope.spawn(move || {
                    // Wait until the synchronous removal has passed its (empty) scan, then put
                    // the entry back and scan it: an ungated `RemoveHook(id)`.
                    hold.wait_for_scanned(Duration::from_secs(5));
                    mock.insert_thread_requeue(id, held.1);
                    // SAFETY: the same call the wrapper makes; the mock answers it.
                    unsafe {
                        mock.remove_hook(
                            id,
                            false,
                            core::ptr::null_mut(),
                            core::ptr::null_mut(),
                        )
                    };
                })
            };
            let out = hook::remove_all_blocking(Duration::from_secs(5));
            assert!(out.is_ok(), "the reconcile's call is Ok: {out:?}");
            // Reached after this thread's synchronous call returned and reconciled: let the
            // stale scan's destructor finish (the probe's own bound is the fallback).
            hold.release();
            stale.join().expect("the stale scan finished");

            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "the stale scan's removal function freed the context (the last reference)"
            );
            assert_eq!(
                hook::lost_removal_count(),
                lost_before,
                "the removal function owned the id: the reconcile must not also account for it"
            );
            assert_eq!(hook::live_context_count(), 0);
            assert_eq!(mock.removal_fn_calls(id), 1);
        });
    });
}

/// A callback probe that takes part in [`ScanHold`]: it counts the context being freed (as
/// [`Probe`] does), reports that the removal function is inside the destructor, and waits (bounded)
/// for the test to let it return — the window in which the reconcile on the other thread runs.
struct RacingProbe {
    drops: Arc<AtomicUsize>,
    hold: Arc<ScanHold>,
}

impl Drop for RacingProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        self.hold.set(|state| state.dropping = true);
        // Bounded: the test thread's `remove_all_blocking` returns from its synchronous call
        // (the scan hold releases it) and reconciles while this waits, so the wait is a wedge,
        // not a sleep barrier. It is many orders of magnitude longer than that reconcile needs.
        let _ = self
            .hold
            .wait(|state| state.released, Duration::from_millis(200));
    }
}

/// A hooked call in flight holds its capsule's `_detour_mutex` **shared** for its whole length
/// (`K:214`). While it does:
///
/// * a synchronous registration that cannot take the capsule is queued for the insert thread
///   instead of failing (`InsertHook`'s `try_lock`, `K:1453-1455`; `__Setup__Hook`'s
///   `if (!async)` fallback, `K:1738-1743`, then the queue, `:1745-1748`), and
/// * a synchronous removal of a hook in that capsule waits for the call to end (the 1 ms retry
///   loop, `K:1596`) and then runs, so nothing is lost — it is only delayed.
///
/// Every wait here is a bounded rendezvous, not a timing assumption.
#[test]
fn a_hooked_call_in_flight_only_delays_the_removals() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        // One address for the whole test: an optimized build may give the same empty `fn` item
        // more than one address (LLVM duplicates the identical body), and KHook keys a capsule by
        // the address it was handed at setup. Casting `hooked` at each site can therefore look up
        // a capsule nobody created.
        let target = hooked as *mut c_void;
        // The threads below capture the address as a `usize` (a raw pointer is not `Send`).
        let target_addr = target as usize;
        // Create the capsule (a fresh address forces a synchronous insert, `K:1712-1717`; it
        // succeeds here because no call is in flight).
        let warm_up = register(mock, target, None);
        assert_eq!(warm_up.remove_sync().expect("removed"), Removal::Removed);

        let (id_tx, id_rx) = std::sync::mpsc::channel::<HookId>();
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        std::thread::scope(|scope| {
            // The remover registers its hook while the capsule is free (a synchronous insert on
            // an existing capsule takes the capsule and succeeds, `K:1738-1743`) and removes it
            // once the call is in flight.
            let remover = scope.spawn(move || {
                let hook = unsafe {
                    h_void::setup_inline(
                        target_addr as *mut c_void,
                        Vec::new(),
                        Vec::new(),
                        None,
                        SetupMode::Sync,
                    )
                }
                .expect("registered while the capsule was free");
                assert!(mock.capsule_has_callback(hook.id()), "the synchronous insert ran");
                id_tx.send(hook.id()).unwrap();
                go_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                hook.remove_sync()
            });

            let id = id_rx.recv_timeout(Duration::from_secs(10)).expect("registered");
            // The hooked call: it takes the capsule's mutex for the length of its body, which is
            // the model of every in-flight hooked call (`K:214,1503`).
            let capsule = mock.capsule_index(target_addr);
            let call = scope.spawn(move || {
                let capsule = mock.capsule(capsule);
                let _in_flight = capsule
                    .callbacks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                held_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            held_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the call never took the capsule");

            // A registration that asks for a synchronous insert now cannot take the capsule: it
            // is queued, not refused and not lost.
            let queued = unsafe {
                h_void::setup_inline(
                    target_addr as *mut c_void,
                    Vec::new(),
                    Vec::new(),
                    None,
                    SetupMode::Sync,
                )
            }
            .expect("a queued registration is still a live hook");
            assert_eq!(mock.queue_len(), 1, "the insert was queued for the insert thread");
            assert!(
                mock.has_queued(queued.id()),
                "the call in flight kept it out of the capsule, so it waits in insert_hooks"
            );
            assert!(mock.is_associated(queued.id()));

            go_tx.send(()).unwrap();
            // The remover is now inside its synchronous removal, blocked on the capsule the call
            // holds (`K:1596`).
            wait_until(Duration::from_secs(10), || mock.sync_removes(id) == 1);
            assert_eq!(
                mock.removal_fn_calls(id),
                0,
                "the removal function has not run while the call is in flight"
            );
            assert!(mock.is_associated(id), "the id is still KHook's");

            // End the call: the removal gets the capsule and runs.
            release_tx.send(()).unwrap();
            call.join().expect("the call finished");
            let removed = remover.join().expect("the remover finished");
            assert_eq!(removed.expect("removed"), Removal::Removed);
            assert_eq!(mock.removal_fn_calls(id), 1);
            assert!(!mock.is_associated(id));

            // The queued registration is removed by the scan, which erases the entry and runs its
            // removal function too.
            let queued_id = queued.id();
            assert_eq!(queued.remove_sync().expect("removed"), Removal::Removed);
            assert_eq!(mock.removal_fn_calls(queued_id), 1);
            assert_eq!(mock.queue_len(), 0);
            assert_eq!(hook::live_context_count(), 0);
        });
    });
}

/// With `lost` permanent, a registry that holds nothing but lost ids must not answer
/// [`Error::KHookNotInstalled`](s2sdk::hook::Error::KHookNotInstalled) for ever when no API is
/// installed: lost ids need nothing from us (KHook no longer holds them), so the drain is simply
/// skipped, and the call reports `Ok` without touching the API.
#[test]
fn remove_all_blocking_without_an_api_but_with_lost_ids_is_ok() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let (hook, _held) = queue_later_registration(mock, hooked as *mut c_void, None);
        hook::remove_all_blocking(Duration::from_secs(5)).expect("reconciled");
        assert_eq!(hook::live_context_count(), 0);
        let lost = hook::lost_removal_count();
        assert!(lost > 0);

        hook::uninstall();
        let out = hook::remove_all_blocking(Duration::ZERO);
        assert!(out.is_ok(), "lost ids alone need no API: {out:?}");
        assert_eq!(hook::lost_removal_count(), lost);
        // Restore the mock for the harness's own drain.
        hook::install(mock);
        drop(hook);
    });
}

// -----------------------------------------------------------------------------------------
// 3. The pre-hook entry: FindOriginal / FindOriginalVirtual
// -----------------------------------------------------------------------------------------

/// A hooked vtable slot answers its pre-hook entry (the value the slot held when the hook was
/// set up, `K:1907-1915`); an unhooked slot answers its current entry (`K:1914`); an unhooked
/// function answers itself (`K:1903-1905`). The wrapper adds nothing but the `None`-when-no-API
/// case and keeps the read lock-free of the gate.
#[test]
fn find_original_virtual_answers_the_pre_hook_entry() {
    with_mock(|mock| {
        unsafe extern "C" fn slot0(_this: *mut c_void) {}
        unsafe extern "C" fn slot1(_this: *mut c_void) {}
        unsafe extern "C" fn slot1_replaced(_this: *mut c_void) {}
        unsafe extern "C" fn inline_fn(_this: *mut c_void) {}

        let mut vtable: Vec<*mut c_void> = vec![
            slot0 as *mut c_void,
            slot1 as *mut c_void,
            slot1_replaced as *mut c_void,
        ];
        let vtable = vtable.as_mut_ptr();
        // SAFETY: `vtable` is this test's three-entry array, the index is in range, and the
        // function has the signature the thunks were generated for.
        let hook = unsafe {
            h_void::setup_virtual(vtable, 1, Vec::new(), Vec::new(), None, SetupMode::Async)
        }
        .expect("the mock accepts the virtual registration");
        // The mock records the entry the slot held at setup: that is the pre-hook entry.
        // SAFETY: as above; the wrapper's contract is a live vtable with `index + 1` entries.
        assert_eq!(
            unsafe { hook::find_original_virtual(vtable, 1) },
            Some(slot1 as *mut c_void),
            "the hooked slot answers the entry it replaced (K:1907-1915)"
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { hook::find_original_virtual(vtable, 0) },
            Some(slot0 as *mut c_void),
            "an unhooked slot answers its current entry (K:1914)"
        );
        assert_eq!(
            hook::find_original(inline_fn as *mut c_void),
            Some(inline_fn as *mut c_void)
        );

        let inline = register(mock, inline_fn as *mut c_void, None);
        assert_ne!(inline.id(), hook.id(), "two registrations, two ids");
        assert_eq!(
            hook::find_original(inline_fn as *mut c_void),
            Some(inline_fn as *mut c_void),
            "an inline hook answers the entry it replaced (K:1897-1905)"
        );
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| matches!(
                    call,
                    Call::FindOriginal(_) | Call::FindOriginalVirtual(..)
                ))
                .count(),
            4,
            "the wrapper forwarded every query to the API"
        );
        drop(inline);
        drop(hook);
    });
}

/// No API, no answer: `find_original`/`find_original_virtual` are the only queries that report
/// the missing API instead of panicking, because there is nobody to ask.
#[test]
fn find_original_without_api_is_none() {
    with_mock(|mock| {
        unsafe extern "C" fn slot0(_this: *mut c_void) {}
        let mut vtable: Vec<*mut c_void> = vec![slot0 as *mut c_void];
        let vtable = vtable.as_mut_ptr();
        hook::uninstall();
        assert_eq!(hook::find_original(slot0 as *mut c_void), None);
        // SAFETY: the vtable is live; the wrapper forwards without reading it when no API is
        // installed.
        assert_eq!(unsafe { hook::find_original_virtual(vtable, 0) }, None);
        // Restore the mock for the harness's own drain.
        hook::install(mock);
    });
}

// -----------------------------------------------------------------------------------------
// 4. The thunk depth counter
// -----------------------------------------------------------------------------------------

/// `in_thunk` is true only while a thunk of this module runs, and `remove_sync` defers inside
/// one, in every build: [`in_thunk`] cannot tell "the target is on this stack" (a caller bug)
/// from "an op legitimately running in another hook's tick", and the asynchronous removal never
/// waits on a capsule or the gate, so handing the hook to `Drop`'s path is always safe. If the
/// call took the synchronous path instead, the mock's removal would answer `Removed` here and
/// this test fails.
#[test]
fn remove_sync_in_a_thunk_defers() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}

        // A real `pre` thunk sets the depth for the length of the dispatch, so a callback can
        // see it (that is what lane E will use to choose the asynchronous removal).
        let saw_in_thunk = Arc::new(AtomicUsize::new(0));
        let pre: Vec<h_void::PreCb> = vec![Arc::new({
            let saw_in_thunk = Arc::clone(&saw_in_thunk);
            move |_call: &mut HookCall, _this: *mut c_void| {
                if hook::in_thunk() {
                    saw_in_thunk.fetch_add(1, Ordering::SeqCst);
                }
                Return::ignore(())
            }
        })];
        let probed = unsafe {
            h_void::setup_inline(
                hooked as *mut c_void,
                pre,
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("registered");
        mock.insert_all_pending();
        assert!(!hook::in_thunk());
        mock.enter(mock.context_of(probed.id()));
        // SAFETY: the mock pushed the registering context and no return value is read for a
        // void signature.
        let _: () = unsafe { h_void::pre(fake_this()) };
        mock.leave();
        assert_eq!(
            saw_in_thunk.load(Ordering::SeqCst),
            1,
            "the callback saw the depth"
        );
        assert!(!hook::in_thunk(), "the guard is balanced on exit");
        drop(probed);
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(hook::live_context_count(), 0);

        // The same depth around a direct `remove_sync`. Every build defers to the asynchronous
        // path `Drop` takes, instead of the synchronous removal that could spin on this call's
        // own capsule.
        let second = register(mock, hooked as *mut c_void, None);
        let id = second.id();
        // Put it in the capsule first (a later registration is queued for the insert thread), so
        // the removal really has to be deferred: a hook `RemoveHook`'s scan would find is removed
        // inside the call, and so is one whose capsule `try_lock` succeeds.
        mock.insert_all_pending();
        assert!(mock.capsule_has_callback(id));
        let out = {
            let _depth = hook::ThunkGuard::enter();
            assert!(hook::in_thunk());
            second.remove_sync()
        };
        let removal = out.expect("no panic anywhere");
        assert_eq!(
            removal,
            Removal::Deferred,
            "every build defers instead of spinning on this call's own capsule"
        );
        // The hook was handed to the asynchronous path, exactly as `Drop` does: KHook's delete
        // thread still owes this module a removal, and the count says so.
        assert_eq!(
            mock.async_removes(id),
            1,
            "the removal was asked for asynchronously (khook.hpp:673)"
        );
        assert_eq!(mock.sync_removes(id), 0, "nothing synchronous was asked of KHook");
        assert_eq!(
            mock.deferred_len(),
            1,
            "the removal waits for KHook's delete thread"
        );
        assert_eq!(
            hook::live_context_count(),
            1,
            "the context is still KHook's until that removal runs"
        );
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(mock.removal_fn_calls(id), 1);
        assert_eq!(hook::live_context_count(), 0);
        assert!(
            !hook::in_thunk(),
            "the guard was dropped normally with the call"
        );

        // With no API installed the removal cannot be asked for at all, and inside a thunk a
        // `Drop` cannot be asked either: the API is checked **before** the in-thunk fallback, so
        // every build answers `KHookNotInstalled` there instead of deferring. Either way nothing
        // KHook holds is freed.
        let third = register(mock, hooked as *mut c_void, None);
        let third_id = third.id();
        hook::uninstall();
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _depth = hook::ThunkGuard::enter();
            assert!(hook::in_thunk());
            third.remove_sync()
        }));
        assert!(
            matches!(out.expect("no panic anywhere"), Err(s2sdk::hook::Error::KHookNotInstalled)),
            "every build answers KHookNotInstalled, like the non-thunk branch"
        );
        assert_eq!(
            hook::live_context_count(),
            1,
            "the context stays KHook's: nothing could be removed"
        );
        assert_eq!(mock.removal_fn_calls(third_id), 0);
        // The hook is still registered in KHook; with the API back the drain removes it.
        hook::install(mock);
        hook::remove_all_blocking(Duration::from_secs(5)).expect("removed");
        assert_eq!(mock.removal_fn_calls(third_id), 1);
        assert_eq!(hook::live_context_count(), 0);
    });
}

// -----------------------------------------------------------------------------------------
// 5. The gate: one setup/removal at a time
// -----------------------------------------------------------------------------------------

/// The mock records how many `Setup*`/`RemoveHook` calls are inside it at once; two threads
/// registering and removing in a loop must never show more than one. The mock arms a rendezvous
/// for the first caller — it waits, bounded, for a second one to be inside at the same moment —
/// so the two threads really try to overlap instead of leaving it to the scheduler. Without
/// `KHOOK_GATE` the second one arrives, the maximum becomes 2, and this test fails.
#[test]
fn gate_serialises_setup_and_remove() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_a(_this: *mut c_void) {}
        unsafe extern "C" fn hooked_b(_this: *mut c_void) {}

        mock.arm_gate_rendezvous();

        std::thread::scope(|scope| {
            for function in [
                hooked_a as *mut c_void as usize,
                hooked_b as *mut c_void as usize,
            ] {
                scope.spawn(move || {
                    for _ in 0..50 {
                        let hook = unsafe {
                            h_void::setup_inline(
                                function as *mut c_void,
                                Vec::new(),
                                Vec::new(),
                                None,
                                SetupMode::Async,
                            )
                        }
                        .expect("registered");
                        // The insert entry is removed by the scan, so the removal runs inside
                        // the synchronous call; either way that call is gated.
                        let removal = hook.remove_sync().expect("removed");
                        assert_eq!(removal, Removal::Removed);
                    }
                });
            }
        });

        assert_eq!(
            mock.max_in_api.load(Ordering::SeqCst),
            1,
            "two setup/removal calls were inside KHook at the same time"
        );
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(mock.queue_len(), 0);
    });
}

// -----------------------------------------------------------------------------------------
// 6. Stress: the race itself, with a real insert thread
// -----------------------------------------------------------------------------------------

/// A tiny seeded xorshift64: deterministic choices, no new crate.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        assert!(seed != 0, "xorshift needs a non-zero state");
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A number in `0..bound`.
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Spin for `n` rounds: the seeded stand-in for "this thread loses the CPU for a moment". A
/// `sleep` would be a timer barrier; the spin keeps the choice in the PRNG's hands.
fn spin(n: u64) {
    for _ in 0..n {
        core::hint::spin_loop();
    }
}

/// How one id's removal met KHook's insert side, classified at the end of the run from this
/// module's **own** removal-path counter ([`hook::__set_removal_probe`]) and the context's
/// destructor — never from the mock's bookkeeping, which only knows what it called.
#[derive(Default, Debug)]
struct Counts {
    /// The removal function ran for an id that was never reconciled, and the context was freed.
    normal: usize,
    /// Reconciled lost, and this module's removal path was never entered for the id.
    lost_clean: usize,
    /// Reconciled lost, and a stale re-queued entry made a later scan enter the removal path
    /// once (the lost check ignores it: the context stays leaked).
    lost_stale: usize,
}

/// Calls into this module's removal path per id, from the test probe.
static REMOVAL_CALLS: Mutex<BTreeMap<HookId, usize>> = Mutex::new(BTreeMap::new());

/// The probe [`hook::__set_removal_probe`] installs: one bump per entry into the removal path.
fn count_removal(id: HookId) {
    *REMOVAL_CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(id)
        .or_insert(0) += 1;
}

/// How often this module's removal path was entered for `id`.
fn removal_calls(id: HookId) -> usize {
    REMOVAL_CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&id)
        .copied()
        .unwrap_or(0)
}

/// The pin the test thread uses to choose the state a *pinned* iteration starts from: the
/// free-running insert thread stops at its next step boundary while `paused` is set, so the pop
/// of a specific id (`K:1607-1613`) is the test thread's own and the removal below sees a state
/// the scheduler cannot change.
#[derive(Default)]
struct GateState {
    /// The test thread wants the insert thread parked at a step boundary.
    paused: bool,
    /// The insert thread is parked (or at the top of a step, about to check `paused`).
    idle: bool,
}

struct InsertGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl InsertGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            changed: Condvar::new(),
        }
    }

    /// Called by the insert thread at the top of every step: park while the test thread holds it.
    fn step(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.idle = true;
        self.changed.notify_all();
        while state.paused {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.idle = false;
    }

    /// Park the insert thread at its next step boundary. Bounded: a thread that never reaches one
    /// is a bug in this file, and the [`StressGuard`] resumes it when the panic unwinds.
    fn pause(&self) {
        let deadline = Duration::from_secs(10);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.paused = true;
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, deadline, |state| !state.idle)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timeout.timed_out() && state.idle,
            "the insert thread never reached a step boundary within {deadline:?}"
        );
    }

    /// Let the insert thread run on (also what the guard does on a failing assert).
    fn resume(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paused = false;
        self.changed.notify_all();
    }
}

/// Sets `stop` when it drops, so a failing assert — which unwinds out of the loop and then leaves
/// `thread::scope` waiting for the insert thread — cannot hang the whole binary (and with it, the
/// other cases behind `TEST_LOCK`). `resume` matters too: a panic while a pinned iteration has
/// the insert thread parked would otherwise leave it waiting for ever.
struct StressGuard<'a> {
    stop: &'a AtomicBool,
    gate: &'a InsertGate,
}

impl Drop for StressGuard<'_> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.gate.resume();
    }
}

/// One pass of KHook's insert thread over the modelled state (`K:1603-1628`): pop the oldest
/// entry under `insert_hooks_mutex`, release it (`K:1613`), `__InsertHook_Sync` (`K:1615`: the
/// capsule's `try_lock`, which a call in flight makes fail at random here), relock and re-queue
/// on failure (`K:1618-1622`). The gate parks the whole step when a pinned plan asks for it; the
/// seeded spins replace the thread's 5 ms pause (`K:1626`).
fn insert_thread_step(mock: &RaceMock, gate: &InsertGate, rng: &mut XorShift) {
    gate.step();
    let Some((id, details)) = mock.insert_thread_pop() else {
        std::thread::yield_now();
        return;
    };
    // In the pop window: the entry is out of `insert_hooks` and not in the capsule yet.
    spin(rng.below(64));
    // A capsule `try_lock` that fails (`K:1453-1455`): a hooked call holds the mutex, or another
    // removal does.
    mock.insert_fails.store(rng.below(4) == 0, Ordering::SeqCst);
    let inserted = mock.insert_thread_apply(id, &details);
    mock.insert_fails.store(false, Ordering::SeqCst);
    if !inserted {
        // `K:1618-1623`: relock `insert_hooks_mutex` and push the same details back. The spin
        // models the time that takes (the thread first has to get the mutex back), and it is
        // deliberately wide: that gap is the window in which a removal can erase the association
        // and reconcile the id, leaving a stale entry behind.
        spin(rng.below(200_000));
        mock.insert_thread_requeue(id, details);
    }
}

/// 1000 iterations of the real race. A **free-running insert thread** (`insert_thread_step`,
/// seeded) works the queue beside the test thread, which registers a hook and then removes it at
/// a seeded random point of that thread's pass: immediately (the pop window), after letting the
/// insert land, or through `remove_all_blocking` (which drops the `Hook` first).
///
/// Plans 1 and 2 are **pinned**: the test thread parks the insert thread at a step boundary
/// (a bounded rendezvous, not a sleep) and takes the entry itself, so state (a) (associated, no
/// capsule callback, nothing queued) or the inserted state is guaranteed before the removal
/// races the free-running thread again. Plan 0 and plan 3 are **free**: the removal races
/// whatever state the insert thread leaves, and their outcome counts are printed but not
/// asserted — a scheduler may legitimately never produce one of them in a run.
///
/// After every iteration: nothing KHook can still call into, the context is either freed exactly
/// once (removal ran) or leaked on purpose (reconciled), and this module's removal path was
/// entered at most once for the id. After the final drain every id is classified with the counts
/// below, and every pinned outcome must have occurred: the stress has to reach the race, not just
/// the healthy path.
#[test]
fn race_stress_1000() {
    with_mock(|mock| {
        mock.recording.store(false, Ordering::SeqCst);
        // Count entries into this module's removal path, not the mock's calls.
        hook::__set_removal_probe(count_removal);
        unsafe extern "C" fn hooked(_this: *mut c_void) {}

        // Create the capsule once, so every iteration's registration is a later one on an
        // existing capsule and is queued for the insert thread (`K:1738-1748`).
        let warm_up = register(mock, hooked as *mut c_void, None);
        assert_eq!(warm_up.remove_sync().expect("removed"), Removal::Removed);

        let mut rng = XorShift::new(0x5EED_1234_ABCD_0001);
        let stop = AtomicBool::new(false);
        let gate = InsertGate::new();
        let deadline_hit = AtomicBool::new(false);
        // `(id, drop probe, reconciled, pinned)`.
        let mut ids: Vec<(HookId, Arc<AtomicUsize>, bool, bool)> = Vec::new();
        // A reconciled id whose details this thread still holds: re-queued before the final drain
        // so the stale-entry path (and the drain that erases it) is really exercised.
        let mut stale_requeues: Vec<(HookId, Details)> = Vec::new();
        let mut pinned = Counts::default();
        let mut free = Counts::default();
        // Counts the pinned pop windows, so half of them re-queue their stale entry and half drop
        // it (`K:1566-1571` / `K:1618-1622`) without depending on the PRNG.
        let mut window_serial = 0usize;

        std::thread::scope(|scope| {
            let _guard = StressGuard {
                stop: &stop,
                gate: &gate,
            };
            let insert_thread = scope.spawn(|| {
                let mut rng = XorShift::new(0x1DEA_5EED_0000_0007);
                while !stop.load(Ordering::SeqCst) {
                    insert_thread_step(mock, &gate, &mut rng);
                }
                // A final pass so the queue is empty for the classification.
                while mock.queue_len() > 0 {
                    insert_thread_step(mock, &gate, &mut rng);
                }
            });

            for _ in 0..1000 {
                let drops = Arc::new(AtomicUsize::new(0));
                let hook = register(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
                let id = hook.id();
                let lost_before = hook::lost_removal_count();
                let iteration_started = Instant::now();

                // The plan decides where the pop window is; the free-running thread races every
                // one of them, and the seeded spin before the removal moves the removal to a
                // different point of its pass each time.
                let plan = rng.below(4);
                // The `(id, details)` this thread took out of `insert_hooks` (the pop window,
                // `K:1607-1613`), kept so a later re-queue can be modelled exactly.
                let mut held_details = None;
                let mut is_pinned = false;
                if plan == 1 || plan == 2 {
                    // A pinned plan: park the insert thread and take the entry itself.
                    gate.pause();
                    let (popped, foreign) = mock.insert_thread_pop_until(id);
                    // Foreign entries are older ids' stale re-queues the free-running thread left
                    // at the head (`K:1607-1610` pops the oldest); they go back at the head in
                    // their own order, so a removal of *this* id does not trip over them
                    // (`left: 112, right: 268` was exactly that).
                    for (foreign_id, foreign_details) in foreign.into_iter().rev() {
                        mock.insert_thread_requeue_front(foreign_id, foreign_details);
                    }
                    match (plan, popped) {
                        (1, Some((_, details))) => {
                            // State (a): associated, no capsule callback, nothing queued.
                            held_details = Some(details);
                            is_pinned = true;
                        }
                        (2, Some((popped, details))) => {
                            // Let the insert land first (`K:1563-1576`): the capsule holds the
                            // callback. If it still fails, re-queue it (`K:1618-1622`) and the
                            // iteration is counted as free rather than pinned.
                            mock.insert_fails.store(false, Ordering::SeqCst);
                            if mock.insert_thread_apply(popped, &details) {
                                is_pinned = true;
                            } else {
                                mock.insert_thread_requeue(popped, details);
                            }
                        }
                        _ => {}
                    }
                    gate.resume();
                }
                spin(rng.below(400));
                let use_all = plan == 3 || (plan == 0 && rng.below(2) == 0);
                if use_all {
                    // The `Drop` queues an asynchronous removal; the drain re-issues it.
                    drop(hook);
                    let out = hook::remove_all_blocking(Duration::from_secs(5));
                    assert!(out.is_ok(), "iteration {id}: the drain is Ok: {out:?}");
                } else {
                    let out = hook.remove_sync().expect("the removal is always answered");
                    assert_ne!(
                        out,
                        Removal::Deferred,
                        "iteration {id}: no thunk is on this thread"
                    );
                }

                if let Some(details) = held_details {
                    // Half the windows drop the details the way the insert thread does when the id
                    // is no longer associated (`K:1566-1571`) — a clean loss. The rest re-queue
                    // them after the loop (`K:1618-1622`), the stale entry the drain has to erase.
                    if window_serial.is_multiple_of(2) {
                        stale_requeues.push((id, details));
                    }
                    window_serial += 1;
                }
                // Let the insert thread run on and settle.
                spin(rng.below(200));

                if iteration_started.elapsed() >= Duration::from_secs(5) {
                    deadline_hit.store(true, Ordering::SeqCst);
                }
                let reconciled = hook::lost_removal_count() > lost_before;
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    usize::from(!reconciled),
                    "iteration {id}: a context is freed exactly once or leaked, never both"
                );
                assert_eq!(
                    hook::live_context_count(),
                    0,
                    "iteration {id}: the removal (or the reconcile) came down to zero"
                );
                assert!(
                    removal_calls(id) <= 1,
                    "iteration {id}: this module's removal path ran twice"
                );
                ids.push((id, Arc::clone(&drops), reconciled, is_pinned));
            }

            stop.store(true, Ordering::SeqCst);
            insert_thread.join().expect("the insert thread finished");
        });

        // The insert thread's stale re-queues (`K:1618-1622`): the details of ids that were
        // reconciled while the thread held them. Nothing takes them out of the queue any more
        // (the insert thread has stopped), so the drain below is the only thing that can.
        for (id, details) in stale_requeues {
            mock.insert_thread_requeue(id, details);
        }
        // Everything a stale entry could still name is drained in one call: each lost id gets one
        // synchronous removal, which erases a re-queued entry and enters the (ignored) removal
        // path.
        let drained = hook::remove_all_blocking(Duration::from_secs(5));
        assert!(drained.is_ok(), "the final drain is Ok: {drained:?}");
        assert_eq!(mock.queue_len(), 0, "no stale entry outlived the drain");
        assert_eq!(hook::live_context_count(), 0);

        for (id, drops, reconciled, is_pinned) in &ids {
            let calls = removal_calls(*id);
            assert!(
                calls <= 1,
                "id {id}: this module's removal path ran {calls} times"
            );
            let bucket = if *is_pinned { &mut pinned } else { &mut free };
            if *reconciled {
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    0,
                    "id {id}: a reconciled context is never freed"
                );
                if calls == 0 {
                    bucket.lost_clean += 1;
                } else {
                    bucket.lost_stale += 1;
                }
            } else {
                assert_eq!(
                    calls, 1,
                    "id {id}: a removed hook entered the removal path exactly once"
                );
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    1,
                    "id {id}: and its context was freed exactly once"
                );
                bucket.normal += 1;
            }
        }

        assert!(
            !deadline_hit.load(Ordering::SeqCst),
            "an iteration waited out a deadline"
        );
        println!(
            "race_stress_1000: pinned normal={} lost_clean={} lost_stale={}",
            pinned.normal, pinned.lost_clean, pinned.lost_stale
        );
        println!(
            "race_stress_1000: free   normal={} lost_clean={} lost_stale={}",
            free.normal, free.lost_clean, free.lost_stale
        );
        // Only the pinned outcomes are asserted: they do not depend on the scheduler.
        assert!(pinned.normal > 0, "the pinned plans never took the normal path");
        assert!(
            pinned.lost_clean > 0,
            "the pinned pop window never reconciled a removal the insert thread held"
        );
        assert!(
            pinned.lost_stale > 0,
            "the pinned pop window never produced a stale re-queue behind a reconcile"
        );
    });
}

// -----------------------------------------------------------------------------------------
// 7. Nothing reachable from a KHook callback may panic
// -----------------------------------------------------------------------------------------

/// The unload-shaped sequence the game dies on: a lost id with a stale re-queued entry that
/// Metamod's per-plugin unloader consumes **after** the plugin's `Unload` has uninstalled the
/// API.
///
/// Real sequence: the plugin's `Unload` calls `remove_all_blocking` (reconcile here), then
/// `khook_api::uninstall`; `~CPlugin` then builds an `Unloader`
/// (`core/metamod_plugins.cpp:46-56,205-213`) that issues `RemoveHook` for every id the plugin
/// ever registered, and for a lost id the scan finds the stale entry the insert thread pushed
/// back (`K:1618-1622`) and calls our removal function (`K:1819-1850`) — with no API installed.
/// That call is `extern "C"`, so a panic there aborts the game. It must instead return on the id
/// alone ([`hook::release_on_removal_by_id`]): the probe records the call, nothing is touched.
#[test]
fn unload_shaped_sequence_ignores_a_stale_removal_call_after_uninstall() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        hook::__set_removal_probe(count_removal);
        let (hook, held) =
            queue_later_registration(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();
        let lost_before = hook::lost_removal_count();

        // Unload: drop first (`K:1852-1869` queues the removal), then the synchronous drain. The
        // insert thread holds the details, so the removal is lost and reconciled.
        drop(hook);
        hook::remove_all_blocking(Duration::from_secs(5)).expect("reconciled");
        assert_eq!(hook::lost_removal_count(), lost_before + 1);
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(mock.removal_fn_calls(id), 0);

        // Metamod unloads the plugin: the IKHook is gone.
        hook::uninstall();

        // The insert thread's failed `__InsertHook_Sync` finally re-queues the details it held
        // (`K:1618-1622`), and the unloader's scan consumes the stale entry.
        mock.insert_thread_requeue(id, held);
        let calls_before = removal_calls(id);
        // SAFETY: the same call KHook's scan makes, with the same null caller slots; the mock
        // answers it. The API is uninstalled, which is the point.
        unsafe { mock.remove_hook(id, false, core::ptr::null_mut(), core::ptr::null_mut()) };

        assert_eq!(
            mock.removal_fn_calls(id),
            1,
            "the stale scan ran this module's removal function"
        );
        assert_eq!(
            removal_calls(id),
            calls_before + 1,
            "the removal path was entered for the stale entry"
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "a reconciled context is never freed"
        );
        assert_eq!(
            hook::live_context_count(),
            0,
            "and the count did not move"
        );
        assert_eq!(
            hook::lost_removal_count(),
            lost_before + 1,
            "no second reconcile"
        );
        assert!(!mock.has_queued(id), "the scan erased the stale entry");

        // Restore the mock for the harness's own drain.
        hook::install(mock);
    });
}

/// A `Hook` dropped from inside a hook thunk with no API installed: `Drop` may not panic (it runs
/// inside KHook's `extern "C"` frame), so it leaks this reference and KHook's and reports. The
/// thunk's depth is what tells it that a synchronous fallback is not an option either.
#[test]
fn drop_from_a_thunk_with_no_api_leaks_and_reports_instead_of_aborting() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        let drops = Arc::new(AtomicUsize::new(0));
        let hook = register(mock, hooked as *mut c_void, Some(Arc::clone(&drops)));
        let id = hook.id();
        assert_eq!(hook::live_context_count(), 1);
        hook::uninstall();

        // The guard stands in for the `pre`/`post` thunk around a callback that drops its hook.
        let depth = hook::ThunkGuard::enter();
        assert!(hook::in_thunk());
        drop(hook);
        drop(depth);

        assert_eq!(mock.removal_fn_calls(id), 0, "nothing could be asked of KHook");
        assert_eq!(
            hook::live_context_count(),
            1,
            "the context stays KHook's: the leak is the point"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0, "and is not freed");
        let lines = take_sink_lines();
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&format!("hook id {id} dropped with no KHook IKHook"))),
            "the missing API was reported: {lines:?}"
        );

        // The hook is still registered in KHook and the registry still owes it a removal: with
        // the API back, the unload drain removes it normally (and frees the context once).        assert!(mock.is_associated(id));
        hook::install(mock);
        hook::remove_all_blocking(Duration::from_secs(5)).expect("removed");
        assert_eq!(mock.removal_fn_calls(id), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(hook::live_context_count(), 0);
    });
}

// -----------------------------------------------------------------------------------------
// 8. The reconcile and a stale removal call, in one critical section
// -----------------------------------------------------------------------------------------

/// The armed id and its [`ScanHold`], shared by the removal probe and the diagnostic sink below.
static STRADDLE_HOLD: Mutex<Option<(HookId, Arc<ScanHold>)>> = Mutex::new(None);

fn straddle_hold() -> Option<(HookId, Arc<ScanHold>)> {
    STRADDLE_HOLD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Removal probe: counts, and for the armed id parks the stale call (bounded) until the reconcile
/// on the other thread has emitted its line.
fn straddle_probe(id: HookId) {
    count_removal(id);
    if let Some((armed, hold)) = straddle_hold() {
        if armed == id {
            hold.set(|state| state.dropping = true);
            let _ = hold.wait(|state| state.released, Duration::from_secs(5));
        }
    }
}

/// Sink: records, and releases the parked stale call once the reconcile line for the armed id is out.
fn straddle_sink(line: &str) {
    sink(line);
    if let Some((armed, hold)) = straddle_hold() {
        if line == format!("khook: lost removal for id {armed} reconciled (KHook insert race)") {
            hold.set(|state| state.released = true);
        }
    }
}

/// A stale scan enters the removal function for `id` BEFORE the reconcile (it is parked at the
/// removal probe) and decides AFTER it. With `admit_removal` in one critical section the decision
/// is taken after the reconcile and is `Ignore`; split into check and claim scopes (the probe
/// between them) the stale call claims nothing, falls to `Unclaimed`, and lowers `live` a second
/// time. That split is the negative this pins: move the probe seam into the gap between the check
/// and the claim and this test fails with `live` at 0 instead of 1 (the canary alone).
#[test]
fn r3_stale_call_straddling_the_reconcile_is_ignored() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        unsafe extern "C" fn canary_target(_this: *mut c_void) {}
        // Keeps `live` above zero so a double decrement shows as a wrong count, not an underflow abort.
        let (hook, details) = queue_later_registration(mock, hooked as *mut c_void, None);
        let canary = register(mock, canary_target as *mut c_void, None);
        let id = hook.id();
        assert_eq!(hook::live_context_count(), 2);
        let hold = Arc::new(ScanHold::new());
        mock.arm_scan_hold(&hold, id);
        *STRADDLE_HOLD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id, Arc::clone(&hold)));
        hook::__set_removal_probe(straddle_probe);
        hook::set_diagnostic_sink(straddle_sink);
        let lost_before = hook::lost_removal_count();
        let calls_before = removal_calls(id);
        let out = std::thread::scope(|scope| {
            let stale = scope.spawn(|| {
                hold.wait_for_scanned(Duration::from_secs(5));
                mock.insert_thread_requeue(id, details);
                // An ungated asynchronous RemoveHook (an in-thunk Drop's): its scan consumes the
                // stale entry and calls the removal function on this thread.
                unsafe { mock.remove_hook(id, true, core::ptr::null_mut(), core::ptr::null_mut()) };
            });
            let out = hook.remove_sync();
            stale.join().expect("stale scan finished");
            out
        });
        *STRADDLE_HOLD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        hook::__set_removal_probe(count_removal);
        hook::set_diagnostic_sink(sink);
        let lines = take_sink_lines();
        assert_eq!(out.expect("remove_sync"), Removal::ReconciledLost);
        assert_eq!(removal_calls(id), calls_before + 1, "the stale call entered once");
        assert_eq!(hook::lost_removal_count(), lost_before + 1);
        assert!(!lines.iter().any(|l| l.contains("underflow")), "{lines:?}");
        assert_eq!(
            hook::live_context_count(),
            1,
            "only the canary is left: the stale call must not lower the count again ({lines:?})"
        );
        drop(canary);
    });
}
