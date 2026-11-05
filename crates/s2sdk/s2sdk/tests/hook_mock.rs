//! Mock-driven tests for [`s2sdk::hook`]: a `KHookApi` mock that records every call with its
//! arguments and simulates KHook's own trampoline (`detour.cpp:746-990`) well enough to drive
//! the thunks the `khook_thunks!` macro generated, on real `extern "C"` functions.
//!
//! Every mock return value and parameter order mirrors a line of `khook.hpp` / `detour.cpp`,
//! cited at the point of use; the mock also asserts the contracts those lines imply (a
//! `SaveReturnValue` pointer is readable for the size it claims, `original = true` only inside
//! an original-call window, `DestroyReturnValue` only with a call in progress, ...).
//!
//! KHook's per-thread state (`g_current_hook`, `g_saved_params`) is a thread-local here, like
//! the original (`detour.cpp:359-361,453-472`). The tests serialize on [`TEST_LOCK`] because
//! the API slot is process-wide ([`s2sdk::hook::install`]) and each case wants a fresh mock;
//! `--test-threads=4` still runs the file, the cases just queue.
//!
//! # What this mock does not model
//!
//! * KHook's per-hooked-address mutex (`detour.cpp:1498-1543` `_detour_mutex`): the trampoline
//!   holds it shared for a whole call and removal needs it exclusively. What the mock *does*
//!   model is the removal half of it: [`Mock::claim_and_run_removal`] serializes the delete
//!   thread and a synchronous removal the way `__RemoveHook_Sync` holds
//!   `associated_hooks_mutex` exclusively around the capsule's `RemoveHook`
//!   (`detour.cpp:1581-1597`), so one hook's removal function never runs twice. The
//!   trampoline's shared hold is not modelled: a *synchronous* removal of a target that is on
//!   the current call stack spins forever in KHook, which cannot be tested without a second
//!   thread stuck in a hook (documented on [`s2sdk::hook::remove_all_blocking`]).
//! * The insert thread's delay (`detour.cpp:1603-1628`): the mock records the `async` flag
//!   `SetupHook` was given and installs the hook anyway, because the wrapper's behaviour does
//!   not depend on when KHook gets to the insertion — only on the flag reaching KHook. The
//!   removal side *is* modelled: an asynchronous removal is queued until the test releases the
//!   mock's delete thread (`detour.cpp:1630-1669`).
//! * `GetCurrentValuePtr(pop)`'s two snapshots (`g_last_loop` against `g_saved_params.top()`,
//!   `detour.cpp:453-464`): the running call is the finished one in a single-threaded
//!   simulation, so `pop` changes nothing here. Only the generated `make_return` asks for
//!   `pop = true`; the callback object's `current_value_ptr()` always reads the running call.
//! * `DoRecall`, `FindOriginalVirtual` and `LookupSignature`: the wrapper never reaches them,
//!   and the mock panics rather than inventing an answer if that changes.

use core::cell::RefCell;
use core::ffi::c_void;
use core::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use s2sdk::hook::{self, Action, Error, HookCall, HookId, KHookApi, Removal, Return, SetupMode};
use s2sdk::khook_thunks;

// -----------------------------------------------------------------------------------------
// The signatures under test
// -----------------------------------------------------------------------------------------

/// A 12-byte `#[repr(C)]` struct (three floats): Itanium's classifier puts the three floats in
/// the SSE class and returns them in `XMM0:XMM1` (`XMM0:XMM1:XMM2` would be the third eightbyte;
/// 12 ≤ 16 bytes so no hidden pointer), while an MSVC non-static member returns *every* class
/// through the hidden pointer after `this`, a 12-byte one included
/// (`msvc_hidden_return_fn`, `gen/gen.py:3332-3360`). So this row is
/// `sret_itanium = no_sret, sret_msvc = sret`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct V3 {
    x: f32,
    y: f32,
    z: f32,
}

/// A 24-byte `#[repr(C)]` struct: both ABIs return it through a hidden pointer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct V24 {
    w: [u64; 3],
}

khook_thunks! {
    /// `void f(this)`.
    pub mod h_void: fn(this: *mut c_void) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `u32 f(this, u32)`.
    pub mod h_u32: fn(this: *mut c_void, a0: val u32) -> u32 {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `V3 f(this, i32, *mut c_void)`.
    pub mod h_v3: fn(this: *mut c_void, a0: val i32, a1: val *mut c_void) -> V3 {
        sret_msvc = sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `V24 f(this, i32)`.
    pub mod h_v24: fn(this: *mut c_void, a0: val i32) -> V24 {
        sret_msvc = sret,
        sret_itanium = sret,
    }
}

khook_thunks! {
    /// `void f(this, u64, u64, u64, u64, u64)`.
    pub mod h_void5: fn(
        this: *mut c_void,
        a0: val u64,
        a1: val u64,
        a2: val u64,
        a3: val u64,
        a4: val u64,
    ) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `f64 f(this, f64)`.
    pub mod h_f64: fn(this: *mut c_void, a0: val f64) -> f64 {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

/// `vec_t q[3]`, `public/mathlib/vector.h:1757-1761`: the 12-byte class a `const QAngle&`
/// parameter refers to. Its *referent's* size is what KHook copies for such a parameter
/// (`khook.hpp:101-102`), which is why a reference parameter needs the `ref` marker.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QAngle {
    x: f32,
    y: f32,
    z: f32,
}

/// `struct CSplitScreenSlot { int m_Data; }`, `public/tier0/basetypes.h:579-601`: a 4-byte
/// class passed **by value**, so its `sizeof` is its own 4 bytes (`8` after the `max` on
/// Linux), not a referent's.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CSplitScreenSlot {
    m_data: i32,
}

khook_thunks! {
    /// `void f(this, const QAngle&)` — a C++ *reference* parameter: `sizeof(ARGS)` is
    /// `sizeof(QAngle)` = 12, not the 8 a pointer would take.
    pub mod h_qangle_ref: fn(this: *mut c_void, ang: ref *const QAngle) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `bool f(this, CSplitScreenSlot, double, QAngle*)` — value, value, pointer.
    pub mod h_bool_mixed: fn(
        this: *mut c_void,
        slot: val CSplitScreenSlot,
        d: val f64,
        angles: val *const QAngle,
    ) -> bool {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `CSplitScreenSlot f(this, i32)` — a **4-byte class** returned by value, the row that
    /// pins the MSVC rule: a non-static member returns every class/struct/union through the
    /// hidden pointer after `this`, a 4-byte one included (`msvc_hidden_return_fn`,
    /// `gen/gen.py:3332-3360`), while Itanium returns a trivially-copyable 4-byte class in
    /// `eax`/`RAX` with no hidden pointer. Declaring `sret_msvc = no_sret` here would be the
    /// old, wrong rule.
    pub mod h_slot4: fn(this: *mut c_void, a0: val i32) -> CSplitScreenSlot {
        sret_msvc = sret,
        sret_itanium = no_sret,
    }
}

// The flags reach the machine signature the way they have to: a 4-byte class return takes the
// hidden buffer on MSVC and comes back in `RAX` on Itanium. Checked at compile time on both
// targets (the windows half by `cargo check --target x86_64-pc-windows-msvc`).
#[cfg(not(windows))]
const _: () = assert!(
    !h_slot4::HIDDEN_RETURN_BUFFER,
    "Itanium returns a 4-byte trivially-copyable class in RAX"
);
#[cfg(windows)]
const _: () = assert!(
    h_slot4::HIDDEN_RETURN_BUFFER,
    "MSVC returns every class through the hidden pointer after `this`"
);
// And the converse, so the const is not simply always false on Itanium: a 24-byte class does
// come back through the hidden buffer there.
#[cfg(not(windows))]
const _: () = assert!(h_v24::HIDDEN_RETURN_BUFFER);
#[cfg(windows)]
const _: () = assert!(h_v24::HIDDEN_RETURN_BUFFER);

// The compile-time half of the declared-vs-computed check: a caller holding a stack size from
// generated IR writes exactly this, and a mismatch is a compile error naming both numbers
// (`s2sdk::hook::assert_stack_size`). The literal is the one `stack_size_table` computes by
// hand, so this is a real cross-check and not the const fn compared with itself.
#[cfg(not(windows))]
const _: u32 = hook::assert_stack_size(24, h_u32::STACK_SIZE);
#[cfg(windows)]
const _: u32 = hook::assert_stack_size(32, h_u32::STACK_SIZE);

// -----------------------------------------------------------------------------------------
// The mock
// -----------------------------------------------------------------------------------------

/// One `SaveReturnValue` call, with the bytes the pointer really held
/// (`detour.cpp:366-406` copies `return_size` bytes through `init_op`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Saved {
    action: Action,
    size: usize,
    ptr_is_null: bool,
    bytes: Vec<u8>,
    original: bool,
}

/// Every call the wrapper made, in order, with the arguments it made them with.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    SetupHook {
        function: usize,
        context: usize,
        removed: usize,
        pre: usize,
        post: usize,
        make_return: usize,
        make_call_original: usize,
        stack_size: u32,
        r#async: bool,
    },
    SetupVirtualHook {
        vtable: usize,
        index: i32,
        context: usize,
        removed: usize,
        pre: usize,
        post: usize,
        make_return: usize,
        make_call_original: usize,
        stack_size: u32,
        r#async: bool,
    },
    RemoveHook {
        id: HookId,
        r#async: bool,
        hook_removal_fn: usize,
        context: usize,
    },
    GetContextPtr,
    GetOriginalFunction,
    GetOriginalValuePtr,
    GetOverrideValuePtr,
    GetCurrentValuePtr(bool),
    DestroyReturnValue,
    SaveReturnValue(Saved),
    WasOriginalFunctionSkipped,
    /// The detour's own removal function, as KHook runs it (`detour.cpp:1536-1541`).
    RemovalCallback(HookId),
}
/// The running call's storage, mirroring `detour.cpp`'s `AsmLoopDetails` fields the thunks
/// can observe through `SaveReturnValue`/`Get*ValuePtr`/`WasOriginalFunctionSkipped`.
#[derive(Clone, Debug)]
struct Loop {
    /// `loop->action` (`detour.cpp:383`): raised, never lowered.
    action: Action,
    /// `loop->override_return_ptr`'s contents.
    override_bytes: Option<Vec<u8>>,
    /// `loop->original_return_ptr`'s contents.
    original_bytes: Option<Vec<u8>>,
    /// `original_call_state == CompleteSkipped` (`detour.cpp:889`).
    original_skipped: bool,
    /// `original_call_state == InProgress` while `make_call_original` runs.
    original_in_progress: bool,
}

impl Default for Loop {
    fn default() -> Self {
        Self {
            action: Action::Ignore,
            override_bytes: None,
            original_bytes: None,
            original_skipped: false,
            original_in_progress: false,
        }
    }
}

/// The park/release state of [`Mock::epilogue`]: `park` is armed by a test, `parked` says a
/// removal function has returned and its thread is still inside KHook's bookkeeping, and
/// `release` lets it continue.
#[derive(Debug, Default)]
struct Epilogue {
    park: bool,
    parked: bool,
    release: bool,
}

/// KHook's per-thread state: the context stack (`g_current_hook`, `detour.cpp:359-361`) and
/// the per-call storage stack (`g_saved_params`, `detour.cpp:367`).
struct Thread {    contexts: Vec<*mut c_void>,
    loops: Vec<Loop>,
    /// The buffer `GetCurrentValuePtr` last handed out, kept alive for the reader.
    handed_out: Option<Vec<u8>>,
    original_fn: *mut c_void,
}

thread_local! {
    static THREAD: RefCell<Thread> = const {
        RefCell::new(Thread {
            contexts: Vec::new(),
            loops: Vec::new(),
            handed_out: None,
            original_fn: core::ptr::null_mut(),
        })
    };
}

/// The mock `IKHook`: records every call, and plays the trampoline's per-call state machine.
///
/// Its removal model is KHook's (`detour.cpp:1581-1669,1812-1877`): a hook is *associated*
/// while KHook's `associated_hooks`/`_callbacks` hold it, its removal function is stored per
/// id, `remove_hook(id, async = false)` runs the removal inside the call, and
/// `remove_hook(id, async = true)` queues it for the mock's delete thread
/// ([`Mock::fire_deferred_removal`]) — which is where the context of a dropped
/// [`hook::Hook`] is freed and where [`hook::live_context_count`] is lowered.
struct Mock {
    calls: Mutex<Vec<Call>>,
    removal_calls: Mutex<Vec<HookId>>,
    /// The context and the removal function each successful registration recorded, by hook id,
    /// as KHook stores them in the hook's own details. `contexts` keeps every registration;
    /// `associated` holds the ids KHook still has.
    contexts: Mutex<Vec<(HookId, usize)>>,
    removed_fns: Mutex<Vec<(HookId, usize)>>,
    /// The hook ids KHook still holds, i.e. `globals().associated_hooks`.
    associated: Mutex<Vec<HookId>>,
    /// Removals queued for the mock's delete thread, the way KHook's asynchronous removal
    /// queues them (`detour.cpp:1852-1867`).
    deferred: Mutex<Vec<HookId>>,
    /// Signalled when a removal is queued, so a test's delete-thread stand-in can wait for work
    /// instead of sleeping.
    deferred_queued: Condvar,
    /// Delete-thread passes that found nothing to do because the hook had already been removed
    /// synchronously (`detour.cpp:1644`, `__RemoveHook_Sync` answering "already gone").
    skipped: Mutex<Vec<HookId>>,
    /// `associated_hooks_mutex` `detour.cpp:1581-1597` holds exclusively around the capsule's
    /// `RemoveHook`, which is what runs a hook's removal function: the delete thread and a
    /// synchronous removal can never run the same hook's removal function, and this gate is that
    /// serialization ([`Mock::claim_and_run_removal`]).
    removal_gate: Mutex<()>,
    /// The window between a removal function returning and KHook's removing thread getting back
    /// to its loop, which [`hook::remove_all_blocking`]'s barrier has to wait out
    /// ([`Mock::park_after_removal`]).
    epilogue: Mutex<Epilogue>,
    epilogue_changed: Condvar,
    /// Set when a synchronous removal arrives while a removal function is parked inside KHook's
    /// bookkeeping: that is the window the drain's barrier has to wait out, and the test asserting
    /// it makes no timing assumption.
    sync_removal_met_parked_epilogue: AtomicBool,
    /// Armed by [`Mock::hold_sync_removal`]: the next synchronous removal for that id stops
    /// inside the mock until the test releases it, so a test can hold [`KHOOK_GATE`] across a
    /// window it controls.
    hold_sync: Mutex<Option<SyncHold>>,
    hold_sync_changed: Condvar,
    /// `SetupVirtualHook` answers [`hook::INVALID_HOOK`] when set (`khook.hpp:2220-2231`).
    fail_virtual: AtomicBool,
    /// The gate-poisoning fixture (`a_poisoned_gate_recovers_for_the_try_path`): set before an
    /// asynchronous removal, the mock panics instead of queuing the entry — so the caller (a
    /// [`hook::Hook`]'s [`Drop`], which waits for the hook gate outside a thunk) unwinds while
    /// holding the gate and poisons it the way a real panic in gated code would.
    panic_async_remove: AtomicBool,
}

/// The state of [`Mock::hold_sync_removal`]: which id is held, whether the removal has reached
/// the hold (so the test can wait for it deterministically), and whether the test released it.
#[derive(Debug, Default)]
struct SyncHold {
    id: HookId,
    entered: bool,
    released: bool,
}

impl Default for Mock {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            removal_calls: Mutex::new(Vec::new()),
            contexts: Mutex::new(Vec::new()),
            removed_fns: Mutex::new(Vec::new()),
            associated: Mutex::new(Vec::new()),
            deferred: Mutex::new(Vec::new()),
            deferred_queued: Condvar::new(),
            skipped: Mutex::new(Vec::new()),
            removal_gate: Mutex::new(()),
            epilogue: Mutex::new(Epilogue::default()),
            epilogue_changed: Condvar::new(),
            sync_removal_met_parked_epilogue: AtomicBool::new(false),
            hold_sync: Mutex::new(None),
            hold_sync_changed: Condvar::new(),
            fail_virtual: AtomicBool::new(false),
            panic_async_remove: AtomicBool::new(false),
        }
    }
}

impl Mock {
    /// Hold the next **synchronous** removal of `id` inside the mock until
    /// [`Mock::release_held_sync_removal`], so a test can keep [`hook::KHOOK_GATE`]-held work
    /// (a `remove_sync` in flight) across a window it controls.
    fn hold_sync_removal(&self, id: HookId) {
        *self
            .hold_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SyncHold {
            id,
            entered: false,
            released: false,
        });
    }

    /// Wait until the held removal has arrived at the hold, `deadline` only as a failure bound.
    fn wait_for_held_sync_removal(&self, deadline: Duration) {
        let (hold, timed_out) = self
            .hold_sync_changed
            .wait_timeout_while(
                self.hold_sync
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                deadline,
                |hold| !hold.as_ref().is_some_and(|hold| hold.entered),
            )
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timed_out.timed_out() && hold.as_ref().is_some_and(|hold| hold.entered),
            "no synchronous removal reached the hold within {deadline:?}"
        );
    }

    /// Let the held removal continue.
    fn release_held_sync_removal(&self) {
        let mut hold = self
            .hold_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(hold) = hold.as_mut() {
            hold.released = true;
        }
        self.hold_sync_changed.notify_all();
    }

    /// Stop inside a synchronous `remove_hook` for `id` when a test armed the hold — the stretch
    /// of `RemoveHook(id, async = false)` the test keeps its caller inside (`detour.cpp:1870-1876`).
    fn wait_at_sync_hold(&self, id: HookId) {
        let mut hold = self
            .hold_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(armed) = hold.as_mut() else {
            return;
        };
        if armed.id != id {
            return;
        }
        armed.entered = true;
        self.hold_sync_changed.notify_all();
        let (hold, timed_out) = self
            .hold_sync_changed
            .wait_timeout_while(hold, Duration::from_secs(10), |hold| {
                !hold.as_ref().is_some_and(|hold| hold.released)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timed_out.timed_out(),
            "the test never released the held synchronous removal for id {id}"
        );
        assert!(hold.is_some(), "the hold disappeared while it was armed");
    }

    fn record(&self, call: Call) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }

    /// Every call so far (a copy: the tests never hold the recorder while calling a thunk).
    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn saves(&self) -> Vec<Saved> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::SaveReturnValue(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// The context one `Setup*Hook` recorded, i.e. what KHook hands back through
    /// `GetContextPtr()` inside a thunk.
    fn context(&self) -> *mut c_void {
        let contexts = self
            .contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            contexts.len(),
            1,
            "this helper stands in for a single registered hook; use context_of for more"
        );
        contexts[0].1 as *mut c_void
    }

    /// The context of one specific hook id, which is what KHook pushes while it removes that
    /// id (`detour.cpp:1531-1541` takes it from the hook's own stored details).
    fn context_of(&self, id: HookId) -> *mut c_void {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(hook_id, _)| *hook_id == id)
            .map(|(_, ctx)| *ctx as *mut c_void)
            .expect("the hook being removed was registered through this mock")
    }

    /// Whether KHook still holds this hook (`globals().associated_hooks`).
    fn is_associated(&self, id: HookId) -> bool {
        self.associated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id)
    }

    /// The ids KHook still holds, in registration order.
    fn associated(&self) -> Vec<HookId> {
        self.associated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many removals are queued for the delete thread.
    fn deferred_len(&self) -> usize {
        self.deferred
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// One delete-thread pass: pop the oldest queued removal and run it, or note that there was
    /// nothing left to do because the hook was already removed synchronously
    /// (`detour.cpp:1634-1648`: `__RemoveHook_Sync(id, false)` then the caller's removal
    /// callback, which this wrapper passes as null).
    fn delete_thread_pass(&self, index: usize) {
        let id = {
            let mut deferred = self
                .deferred
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                index < deferred.len(),
                "the delete thread was asked for entry {index} of {}",
                deferred.len()
            );
            deferred.remove(index)
        };
        if !self.claim_and_run_removal(id) {
            self.skipped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(id);
        }
    }

    /// Run one hook's removal the way KHook does: under `associated_hooks_mutex` held
    /// exclusively, which `__RemoveHook_Sync` keeps around the capsule's `RemoveHook` while the
    /// latter runs the removal function (`detour.cpp:1581-1597,1536-1541`). Whoever holds the
    /// gate claims the id first; the loser finds it unassociated and runs nothing. `false` means
    /// this call did not run the removal (the hook was already gone).
    fn claim_and_run_removal(&self, id: HookId) -> bool {
        let _gate = self
            .removal_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.claim_and_run_removal_locked(id)
    }

    /// [`Mock::claim_and_run_removal`] for a caller that already holds the gate (the synchronous
    /// removal path, which has to take it even for an id KHook no longer holds).
    fn claim_and_run_removal_locked(&self, id: HookId) -> bool {
        if !self.is_associated(id) {
            return false;
        }
        let context = self.context_of(id);
        self.unassociate(id);
        self.run_removal(id, context);
        self.maybe_park_after_removal();
        true
    }

    /// Stand in for KHook's own work between a removal function returning and the removing thread
    /// getting back to its loop: when a test has armed [`Mock::park_after_removal`], the thread
    /// that just ran a removal function parks here *while holding the removal gate* — i.e. still
    /// inside the bookkeeping `remove_all_blocking`'s barrier has to wait for — until the test
    /// releases it.
    fn maybe_park_after_removal(&self) {
        let mut epilogue = self
            .epilogue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !epilogue.park {
            return;
        }
        epilogue.parked = true;
        self.epilogue_changed.notify_all();
        let (mut epilogue, _) = self
            .epilogue_changed
            .wait_timeout_while(epilogue, Duration::from_secs(10), |e| !e.release)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(epilogue.release, "the test never released the parked removal");
        epilogue.park = false;
        epilogue.release = false;
        epilogue.parked = false;
        // Whoever waits for `parked` to clear has to be woken, not left to its deadline.
        self.epilogue_changed.notify_all();
    }

    /// Arm the park: the next removal function to run on any thread parks after returning
    /// ([`Mock::maybe_park_after_removal`]).
    fn park_after_removal(&self) {
        self.epilogue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .park = true;
    }

    /// Wait until a removal is parked, with `deadline` only as a failure bound.
    fn wait_for_parked_removal(&self, deadline: Duration) {
        let (epilogue, timed_out) = self
            .epilogue_changed
            .wait_timeout_while(
                self.epilogue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                deadline,
                |e| !e.parked,
            )
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timed_out.timed_out() && epilogue.parked,
            "no removal reached the park within {deadline:?}"
        );
    }

    /// Wait until a synchronous removal has arrived at the gate while a removal was parked — the
    /// barrier `remove_all_blocking` issues — with `deadline` only as a failure bound.
    fn wait_for_sync_removal_at_the_parked_epilogue(&self, deadline: Duration) {
        let (_epilogue, timed_out) = self
            .epilogue_changed
            .wait_timeout_while(
                self.epilogue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                deadline,
                |_| !self.sync_removal_met_parked_epilogue.load(Ordering::SeqCst),
            )
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timed_out.timed_out() && self.sync_removal_met_parked_epilogue.load(Ordering::SeqCst),
            "no synchronous removal arrived while the removal was parked within {deadline:?}"
        );
    }

    /// Let a parked removal finish.
    fn release_parked_removal(&self) {
        let mut epilogue = self
            .epilogue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        epilogue.release = true;
        self.epilogue_changed.notify_all();
        let (epilogue, timed_out) = self
            .epilogue_changed
            .wait_timeout_while(epilogue, Duration::from_secs(10), |e| e.parked)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timed_out.timed_out() && !epilogue.parked,
            "the parked removal did not resume"
        );
    }

    /// Wait for the oldest queued removal and run it the way KHook's delete thread would
    /// (`detour.cpp:1630-1669`). Waits on a signal, with `deadline` only as a failure bound.
    fn fire_deferred_removal(&self, deadline: Duration) {
        let mut deferred = self
            .deferred
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, timed_out) = self
            .deferred_queued
            .wait_timeout_while(deferred, deadline, |d| d.is_empty())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        deferred = guard;
        assert!(
            !timed_out.timed_out() && !deferred.is_empty(),
            "no removal reached the mock's delete thread within {deadline:?}"
        );
        drop(deferred);
        self.delete_thread_pass(0);
    }

    /// The delete-thread passes that found their hook already removed.
    fn skipped_removals(&self) -> Vec<HookId> {
        self.skipped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Forget a hook, as KHook does before it calls the removal function
    /// (`detour.cpp:1507-1541` erases from `_callbacks` first).
    fn unassociate(&self, id: HookId) {
        self.associated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|held| *held != id);
    }

    /// What KHook does when it removes a hook: push that hook's context and call the removal
    /// function it stored at setup (`detour.cpp:1531-1541`), then note that it happened.
    fn run_removal(&self, id: HookId, context: *mut c_void) {
        let removed = self
            .removed_fns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(hook_id, _)| *hook_id == id)
            .map(|(_, f)| *f)
            .expect("removal for a hook that was never registered");
        let f: unsafe extern "C" fn(HookId) = unsafe { core::mem::transmute(removed) };
        THREAD.with(|t| t.borrow_mut().contexts.push(context));
        // SAFETY: the wrapper registered this thunk with exactly this signature
        // (`khook.hpp:711-719`), and the mock is standing in for the KHook that calls it.
        unsafe { f(id) };
        THREAD.with(|t| {
            t.borrow_mut().contexts.pop();
        });
        self.record(Call::RemovalCallback(id));
        self.removal_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id);
    }

    /// Start a hooked call: KHook pushes the hook's context (`detour.cpp:755`) and its
    /// per-call storage (`detour.cpp:267-271`) before entering the first thunk.
    fn enter(&self, original_fn: *mut c_void) {
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            t.contexts.push(self.context());
            t.loops.push(Loop::default());
            t.original_fn = original_fn;
        });
    }

    /// End the call, asserting KHook's bookkeeping ended balanced: `DestroyReturnValue` must
    /// have popped the call's storage (`detour.cpp:408-433`) and every context push has been
    /// matched.
    fn leave(&self) {
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            assert!(
                t.loops.is_empty(),
                "the trampoline simulation still has {} unpopped call state: DestroyReturnValue \
                 did not run exactly once",
                t.loops.len()
            );
            assert_eq!(t.contexts.len(), 1, "the hook's context is still pushed by enter()");
            t.contexts.pop();
            t.handed_out = None;
            t.original_fn = core::ptr::null_mut();
        });
    }

    /// The action the current call's `pre` loop settled on (`detour.cpp:886-892`).
    fn current_action(&self) -> Action {
        THREAD.with(|t| {
            t.borrow()
                .loops
                .last()
                .expect("a call is in progress")
                .action
        })
    }

    /// The trampoline's choice: a `Supersede` action skips the original and records
    /// `CompleteSkipped` (`detour.cpp:889`).
    fn skip_original(&self) {
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            let loop_ = t.loops.last_mut().expect("a call is in progress");
            loop_.original_skipped = true;
        });
    }

    /// Open the original-call window the trampoline sets before jumping into
    /// `make_call_original` (`detour.cpp:894`): only inside it is
    /// `SaveReturnValue(original = true)` legal (`detour.cpp:370-372`).
    fn begin_original_call(&self) {
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            t.loops
                .last_mut()
                .expect("a call is in progress")
                .original_in_progress = true;
        });
    }

    /// Close it again once `make_call_original` returned (`detour.cpp:912`).
    fn end_original_call(&self) {
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            t.loops
                .last_mut()
                .expect("a call is in progress")
                .original_in_progress = false;
        });
    }

    /// How many `DestroyReturnValue` calls the mock saw.
    fn destroys(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, Call::DestroyReturnValue))
            .count()
    }

    fn removal_calls(&self) -> Vec<HookId> {
        self.removal_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The `RemoveHook` calls made for hooks *of this mock*, as `(id, async)` — the barrier
    /// [`hook::remove_all_blocking`] issues before reporting `Ok` names [`hook::INVALID_HOOK`],
    /// which is no hook at all, and is counted by [`Mock::barriers`] instead.
    fn removals(&self) -> Vec<(HookId, bool)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::RemoveHook { id, r#async, .. } if id != hook::INVALID_HOOK => {
                    Some((id, r#async))
                }
                _ => None,
            })
            .collect()
    }

    /// How many barrier removals (`INVALID_HOOK`, synchronous) the drain issued.
    fn barriers(&self) -> usize {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::RemoveHook { id, r#async: false, .. } if *id == hook::INVALID_HOOK))
            .count()
    }
}

impl KHookApi for Mock {
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
        self.record(Call::SetupHook {
            function: function as usize,
            context: context as usize,
            removed: removed_function as usize,
            pre: pre as usize,
            post: post as usize,
            make_return: make_return as usize,
            make_call_original: make_call_original as usize,
            stack_size,
            r#async,
        });
        // KHook needs a removal function to tell the hook object it is gone
        // (`detour.cpp:1536-1541`); a null one here would mean the wrapper forgot it.
        assert!(!removed_function.is_null(), "SetupHook without a removal function");
        assert!(!pre.is_null() && !post.is_null());
        assert!(!make_return.is_null() && !make_call_original.is_null());
        assert!(!context.is_null());
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst) + 1;
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, context as usize));
        self.removed_fns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, removed_function as usize));
        self.associated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id);
        id
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
        self.record(Call::SetupVirtualHook {
            vtable: vtable as usize,
            index,
            context: context as usize,
            removed: removed_function as usize,
            pre: pre as usize,
            post: post as usize,
            make_return: make_return as usize,
            make_call_original: make_call_original as usize,
            stack_size,
            r#async,
        });
        if self.fail_virtual.load(Ordering::SeqCst) {
            // What KHook returns when it cannot set the hook up (`khook.hpp:2229`).
            return hook::INVALID_HOOK;
        }
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst) + 1;
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, context as usize));
        self.removed_fns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, removed_function as usize));
        self.associated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id);
        id
    }

    unsafe fn remove_hook(
        &self,
        id: HookId,
        r#async: bool,
        hook_removal_fn: *mut c_void,
        context: *mut c_void,
    ) {
        self.record(Call::RemoveHook {
            id,
            r#async,
            hook_removal_fn: hook_removal_fn as usize,
            context: context as usize,
        });
        // The gate-poisoning fixture: panic while the caller's gate guard is alive, before
        // anything is queued, so the unwind leaves the hook gate poisoned in the real state a
        // panic in gated code produces.
        if r#async && self.panic_async_remove.load(Ordering::SeqCst) {
            panic!("the mock panics inside an asynchronous removal while the caller holds the gate");
        }
        // KHook answers a removal for an id it no longer holds without running anything
        // (`detour.cpp:1853-1860` checks `associated_hooks` first), and it calls the removal
        // function with the *registering* context pushed, taken from the hook's stored details
        // rather than from RemoveHook's own `context` argument (the C++ templates pass null
        // there, as this wrapper does).
        if r#async {
            // An asynchronous removal only queues the work; the removal function runs later,
            // on KHook's delete thread (`detour.cpp:1852-1867`). An id KHook no longer holds is
            // left alone.
            if !self.is_associated(id) {
                return;
            }
            self.deferred
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(id);
            self.deferred_queued.notify_all();
            return;
        }
        // Synchronous: KHook removes the hook and runs the removal function before it returns
        // (`detour.cpp:1870-1876`). Every synchronous removal takes KHook's bookkeeping
        // exclusively anyway — `insert_hooks_mutex` for the not-yet-inserted scan
        // (`detour.cpp:1819`) and then `associated_hooks_mutex` in `__RemoveHook_Sync`
        // (`:1581-1597`) — whether or not it finds the hook, which is what makes one a barrier
        // against a removal function that is still on its way back into KHook.
        if self
            .epilogue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .parked
        {
            self.sync_removal_met_parked_epilogue
                .store(true, Ordering::SeqCst);
            self.epilogue_changed.notify_all();
        }
        let _gate = self
            .removal_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A test can keep this call inside the mock (with the wrapper's `KHOOK_GATE` still
        // held) while another thread drops a `Hook`: exactly the interleaving the gate-free
        // `Drop` exists for.
        self.wait_at_sync_hold(id);
        if !self.is_associated(id) {
            // "Not associated" (`detour.cpp:1582-1585`): KHook answers the removal with `true`
            // and calls nothing.
            return;
        }
        self.claim_and_run_removal_locked(id);
    }

    fn get_context_ptr(&self) -> *mut c_void {
        self.record(Call::GetContextPtr);
        THREAD.with(|t| t.borrow().contexts.last().copied().unwrap_or(
            core::ptr::null_mut(),
        ))
    }

    fn get_original_function(&self) -> *mut c_void {
        self.record(Call::GetOriginalFunction);
        THREAD.with(|t| t.borrow().original_fn)
    }

    fn get_original_value_ptr(&self) -> *mut c_void {
        self.record(Call::GetOriginalValuePtr);
        THREAD.with(|t| {
            let t = t.borrow();
            match t.loops.last().and_then(|l| l.original_bytes.as_ref()) {
                Some(bytes) => bytes.as_ptr().cast_mut().cast(),
                None => core::ptr::null_mut(),
            }
        })
    }

    fn get_override_value_ptr(&self) -> *mut c_void {
        self.record(Call::GetOverrideValuePtr);
        THREAD.with(|t| {
            let t = t.borrow();
            match t.loops.last().and_then(|l| l.override_bytes.as_ref()) {
                Some(bytes) => bytes.as_ptr().cast_mut().cast(),
                None => core::ptr::null_mut(),
            }
        })
    }

    fn get_current_value_ptr(&self, pop: bool) -> *mut c_void {
        self.record(Call::GetCurrentValuePtr(pop));
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            // `detour.cpp:453-468`: the override value when the action reached Override,
            // the original's otherwise. `pop` selects the finished call's snapshot rather
            // than the running one; here the running one is also the finished one.
            let chosen = t.loops.last().and_then(|l| {
                if l.action >= Action::Override {
                    l.override_bytes.clone()
                } else {
                    l.original_bytes.clone()
                }
            });
            match chosen {
                Some(bytes) => {
                    t.handed_out = Some(bytes);
                    t.handed_out
                        .as_ref()
                        .map(|b| b.as_ptr().cast_mut().cast())
                        .unwrap_or(core::ptr::null_mut())
                }
                None => core::ptr::null_mut(),
            }
        })
    }

    fn destroy_return_value(&self) {
        self.record(Call::DestroyReturnValue);
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            assert!(
                !t.loops.is_empty(),
                "DestroyReturnValue with no call in progress (detour.cpp:408-433 pops one)"
            );
            t.loops.pop();
            t.handed_out = None;
        });
    }

    fn find_original(&self, _function: *mut c_void) -> *mut c_void {
        THREAD.with(|t| t.borrow().original_fn)
    }

    fn find_original_virtual(&self, _vtable: *mut *mut c_void, _index: i32) -> *mut c_void {
        core::ptr::null_mut()
    }

    unsafe fn do_recall(
        &self,
        _action: Action,
        _ptr_to_return: *mut c_void,
        _return_size: usize,
        _init_op: *mut c_void,
        _deinit_op: *mut c_void,
    ) -> *mut c_void {
        unreachable!("the wrapper never recalls a hook")
    }

    unsafe fn save_return_value(
        &self,
        action: Action,
        ptr_to_return: *mut c_void,
        return_size: usize,
        init_op: *mut c_void,
        deinit_op: *mut c_void,
        original: bool,
    ) {
        // The generated thunk must hand a readable pointer for the size it claims
        // (`detour.cpp:373-380,397-404` only ever skips the copy when the size is 0).
        assert!(
            return_size == 0 || !ptr_to_return.is_null(),
            "SaveReturnValue with size {return_size} and a null pointer"
        );
        // A zero-sized save must not claim copy operators (`khook.hpp:335-338`).
        if return_size == 0 {
            assert!(init_op.is_null() && deinit_op.is_null());
        } else {
            assert!(!init_op.is_null() && !deinit_op.is_null());
        }
        let bytes = if return_size == 0 {
            Vec::new()
        } else {
            // SAFETY: the assertion above is the contract the thunk has to honour; reading it
            // is what makes a wrong pointer or size fail the test.
            unsafe {
                core::slice::from_raw_parts(ptr_to_return.cast::<u8>(), return_size).to_vec()
            }
        };
        THREAD.with(|t| {
            let mut t = t.borrow_mut();
            let loop_ = t
                .loops
                .last_mut()
                .expect("SaveReturnValue with no call in progress");
            if original {
                // `detour.cpp:370-372` aborts on an original save outside the window
                // `make_call_original` opens (`:894`).
                assert!(
                    loop_.original_in_progress,
                    "SaveReturnValue(original = true) outside the original call window"
                );
                loop_.original_bytes = Some(bytes.clone());
            } else if action > loop_.action {
                // `detour.cpp:383-405`: a strictly higher action replaces the override value.
                loop_.action = action;
                if return_size != 0 {
                    loop_.override_bytes = Some(bytes.clone());
                }
            }
        });
        self.record(Call::SaveReturnValue(Saved {
            action,
            size: return_size,
            ptr_is_null: ptr_to_return.is_null(),
            bytes,
            original,
        }));
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
        self.record(Call::WasOriginalFunctionSkipped);
        THREAD.with(|t| {
            t.borrow()
                .loops
                .last()
                .expect("a call is in progress")
                .original_skipped
        })
    }
}

/// Serializes the cases: the API slot is process-wide and each case installs its own mock.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// KHook's hook-id counter: one global of the library (`detour.cpp:1546-1548`, `++` at `:1728-1729`
/// under `hook_id_mutex`), not a counter per `IKHook`. It is process-wide here for the same reason
/// it is in KHook — and because the wrapper's `lost`, `removal_fired` and
/// `removed_before_registration` sets never shrink: an id must never be handed out twice in this
/// binary, or a later case's registration would match an earlier case's record.
static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// Run one case with a fresh mock installed. Before the case, the live-context count has to be
/// zero (it is process-wide, so a case that leaves one behind would corrupt the next); after
/// it, the mock's delete thread drains everything KHook queued for it — as the real one
/// eventually does (`detour.cpp:1630-1669`) — and the case must end with nothing of this module
/// left for KHook to call into.
fn with_mock<R>(body: impl FnOnce(&'static Mock) -> R) -> R {
    let _guard: MutexGuard<'_, ()> = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        hook::live_context_count(),
        0,
        "a previous case left hook contexts KHook can still call into"
    );
    let mock: &'static Mock = Box::leak(Box::new(Mock::default()));
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
        "every removal function ran by the end of the case"
    );
    hook::uninstall();
    out
}

/// The hook's `this`: a stack object, only ever passed through.
fn fake_this() -> *mut c_void {
    static THIS: AtomicUsize = AtomicUsize::new(0x5A5A_5A5A);
    (&THIS as *const AtomicUsize).cast_mut().cast()
}

// -----------------------------------------------------------------------------------------
// 1. The stack-size table, computed by hand from khook.hpp:83-104
// -----------------------------------------------------------------------------------------

/// `khook.hpp:83-95` written out: `param_count` counts `this` and every argument, a non-void
/// return adds one more slot ("better to copy more than not enough"), the first four machine
/// words go in registers and the `+ 32` is the shadow space KHook's own calls need.
fn win64_expected(param_count: u32, non_void_return: bool) -> u32 {
    let n = param_count + if non_void_return { 1 } else { 0 };
    let stack_args = n.saturating_sub(4);
    stack_args * 8 + 32
}

/// `khook.hpp:96-103` written out: every argument takes at least a pointer slot, plus one for
/// a non-void return.
fn linux_expected(arg_sizes: &[u32], return_size: Option<u32>) -> u32 {
    let ret = match return_size {
        Some(size) if size > 8 => size,
        Some(_) => 8,
        None => 0,
    };
    arg_sizes.iter().map(|size| (*size).max(8)).sum::<u32>() + ret
}

#[test]
fn stack_size_table() {
    // 1. `void(this)`: Win64 n = 1 (this) + 0 args (+0, void) = 1 -> 1 <= 4 -> 0*8 + 32 = 32.
    //    Linux: max(8, 8) = 8 for `this`, no return slot -> 8.
    assert_eq!(h_void::STACK_SIZE_WIN64, win64_expected(1, false));
    assert_eq!(h_void::STACK_SIZE_LINUX, linux_expected(&[8], None));
    assert_eq!(h_void::STACK_SIZE_WIN64, 32);
    assert_eq!(h_void::STACK_SIZE_LINUX, 8);

    // 2. `u32(this, u32)`: Win64 n = 1 + 1 (+1, non-void) = 3 -> 3 <= 4 -> 32.
    //    Linux: max(8, 8) + max(8, 4) = 16, + max(8, 4) = 8 for the return -> 24.
    assert_eq!(h_u32::STACK_SIZE_WIN64, win64_expected(2, true));
    assert_eq!(h_u32::STACK_SIZE_LINUX, linux_expected(&[8, 4], Some(4)));
    assert_eq!(h_u32::STACK_SIZE_WIN64, 32);
    assert_eq!(h_u32::STACK_SIZE_LINUX, 24);

    // 3. `V3(this, i32, *mut c_void)`: Win64 n = 1 + 2 (+1) = 4 -> 4 <= 4 -> 32.
    //    Linux: 8 + 8 + 8 = 24, + max(8, 12) = 12 -> 36.
    assert_eq!(h_v3::STACK_SIZE_WIN64, win64_expected(3, true));
    assert_eq!(
        h_v3::STACK_SIZE_LINUX,
        linux_expected(&[8, 4, 8], Some(12))
    );
    assert_eq!(h_v3::STACK_SIZE_WIN64, 32);
    assert_eq!(h_v3::STACK_SIZE_LINUX, 8 + 8 + 8 + 12);

    // 4. `V24(this, i32)`: Win64 n = 1 + 1 (+1) = 3 -> 32.
    //    Linux: 8 + 8 = 16, + max(8, 24) = 24 -> 40.
    assert_eq!(h_v24::STACK_SIZE_WIN64, win64_expected(2, true));
    assert_eq!(h_v24::STACK_SIZE_LINUX, linux_expected(&[8, 4], Some(24)));
    assert_eq!(h_v24::STACK_SIZE_LINUX, 8 + 8 + 24);

    // 5. `void(this, u64 x5)`: Win64 n = 1 + 5 (+0) = 6 -> (6 - 4)*8 + 32 = 48.
    //    Linux: 6 * max(8, 8) = 48, no return slot -> 48.
    assert_eq!(h_void5::STACK_SIZE_WIN64, win64_expected(6, false));
    assert_eq!(
        h_void5::STACK_SIZE_LINUX,
        linux_expected(&[8, 8, 8, 8, 8, 8], None)
    );
    assert_eq!(h_void5::STACK_SIZE_WIN64, (6 - 4) * 8 + 32);
    assert_eq!(h_void5::STACK_SIZE_LINUX, 6 * 8);
    assert_eq!(h_void5::STACK_SIZE_WIN64, 48);
    assert_eq!(h_void5::STACK_SIZE_LINUX, 48);

    // 6. `f64(this, f64)`: Win64 n = 1 + 1 (+1) = 3 -> 32.
    //    Linux: 8 + 8 = 16, + max(8, 8) = 8 -> 24.
    assert_eq!(h_f64::STACK_SIZE_WIN64, win64_expected(2, true));
    assert_eq!(h_f64::STACK_SIZE_LINUX, linux_expected(&[8, 8], Some(8)));
    assert_eq!(h_f64::STACK_SIZE_WIN64, 32);
    assert_eq!(h_f64::STACK_SIZE_LINUX, 8 + 8 + 8);

    // 7. `void(this, const QAngle&)`: the reference is the round-2 case. Win64 n = 1 + 1 (+0) = 2
    //    -> 2 <= 4 -> 32 (a reference is one machine argument whatever its referent's size).
    //    Linux: `max(8, sizeof(void*))` + `max(8, sizeof(QAngle&))`; `sizeof(QAngle&)` is
    //    `sizeof(QAngle)` = 3 * 4 = 12 ([expr.sizeof]/2), so 8 + 12 = 20. Marking it `val`
    //    (a pointer) would compute 16 and under-copy the caller's stack by 4 bytes.
    assert_eq!(h_qangle_ref::STACK_SIZE_WIN64, win64_expected(2, false));
    assert_eq!(
        h_qangle_ref::STACK_SIZE_LINUX,
        linux_expected(&[8, size_of::<QAngle>() as u32], None)
    );
    assert_eq!(size_of::<QAngle>(), 12, "vec_t x, y, z");
    assert_eq!(h_qangle_ref::STACK_SIZE_WIN64, 32);
    assert_eq!(h_qangle_ref::STACK_SIZE_LINUX, 8 + 12);

    // 8. `bool(this, CSplitScreenSlot, double, QAngle*)`: Win64 n = 1 (this) + 3 args + 1
    //    (non-void return) = 5 -> (5 - 4) * 8 + 32 = 40.
    //    Linux: max(8, 4) + max(8, 8) + max(8, 8) = 24 for the three arguments plus
    //    max(8, 8) = 8 for `this` = 32, plus max(8, sizeof(bool) = 1) = 8 for the return = 40.
    assert_eq!(h_bool_mixed::STACK_SIZE_WIN64, win64_expected(4, true));
    assert_eq!(
        h_bool_mixed::STACK_SIZE_LINUX,
        linux_expected(
            &[
                8,
                size_of::<CSplitScreenSlot>() as u32,
                size_of::<f64>() as u32,
                size_of::<*const QAngle>() as u32,
            ],
            Some(size_of::<bool>() as u32),
        )
    );
    assert_eq!(size_of::<CSplitScreenSlot>(), 4, "int m_Data");
    assert_eq!(h_bool_mixed::STACK_SIZE_WIN64, 40);
    assert_eq!(h_bool_mixed::STACK_SIZE_LINUX, 8 + 8 + 8 + 8 + 8);
    assert_eq!(h_bool_mixed::STACK_SIZE_WIN64, h_bool_mixed::STACK_SIZE_LINUX);

    // 9. `CSplitScreenSlot f(this, i32)` — a 4-byte *class* return: Win64 n = 1 (this) + 1 arg
    //    + 1 (non-void return) = 3 -> 3 <= 4 -> 0 * 8 + 32 = 32. Linux: `max(8, sizeof(void*))`
    //    + `max(8, sizeof(i32))` = 16, plus `max(8, sizeof(CSplitScreenSlot) = 4)` = 8 for the
    //    return -> 24. Both formulas count exactly one return slot whatever the ABI does with
    //    it (MSVC hands it the hidden pointer as the second argument, Itanium returns it in
    //    `eax`), which is why the flag does not change these numbers.
    assert_eq!(h_slot4::STACK_SIZE_WIN64, win64_expected(2, true));
    assert_eq!(
        h_slot4::STACK_SIZE_LINUX,
        linux_expected(&[8, size_of::<i32>() as u32], Some(size_of::<CSplitScreenSlot>() as u32))
    );
    assert_eq!(h_slot4::STACK_SIZE_WIN64, 32);
    assert_eq!(h_slot4::STACK_SIZE_LINUX, 8 + 8 + 8);
    assert_eq!(
        h_slot4::STACK_SIZE_WIN64,
        h_u32::STACK_SIZE_WIN64,
        "`u32` and a 4-byte class take the same slots on Win64"
    );
    assert_eq!(
        h_slot4::STACK_SIZE_LINUX,
        h_u32::STACK_SIZE_LINUX,
        "and the same ones on Linux"
    );

    // `STACK_SIZE` is the computed one for whichever ABI this build targets.
    #[cfg(not(windows))]
    assert_eq!(h_u32::STACK_SIZE, h_u32::STACK_SIZE_LINUX);
    #[cfg(windows)]
    assert_eq!(h_u32::STACK_SIZE, h_u32::STACK_SIZE_WIN64);
}

/// `void f(this, const QAngle&)`: the `ref` marker changes the computed *stack size* and
/// nothing else. The thunks keep the declared Rust signature (a plain `*const QAngle`, which is
/// what a reference parameter mirrors), the declared stack size from the same module is
/// accepted because it agrees with the computation, and the callback sees the caller's object.
#[test]
fn reference_parameter_thunks_keep_the_declared_signature() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_qangle(_this: *mut c_void, _ang: *const QAngle) {}

        let seen = Arc::new(AtomicUsize::new(0));
        let pre: Vec<h_qangle_ref::PreCb> = vec![Arc::new({
            let seen = Arc::clone(&seen);
            move |_call: &mut HookCall, _this: *mut c_void, ang: *const QAngle| {
                seen.store(ang as usize, Ordering::SeqCst);
                Return::supersede_with(())
            }
        })];
        let hook = unsafe {
            h_qangle_ref::setup_inline(
                hooked_qangle as *mut c_void,
                pre,
                Vec::new(),
                Some(h_qangle_ref::STACK_SIZE),
                SetupMode::Async,
            )
        }
        .expect("the computed and declared stack sizes agree");

        let angles = QAngle {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };
        mock.enter(hooked_qangle as *mut c_void);
        let _: () = unsafe { h_qangle_ref::pre(fake_this(), &angles) };
        assert_eq!(mock.current_action(), Action::Supersede);
        mock.skip_original();
        let _: () = unsafe { h_qangle_ref::post(fake_this(), &angles) };
        let _: () = unsafe { h_qangle_ref::make_return(fake_this(), &angles) };
        mock.leave();

        assert_eq!(
            seen.load(Ordering::SeqCst),
            core::ptr::from_ref(&angles) as usize,
            "the callback got the pointer the caller passed"
        );
        assert_ne!(
            hook.id(),
            hook::INVALID_HOOK,
            "KHook answered a live id (the counter is process-wide, `detour.cpp:1548`)"
        );
    });
}

// -----------------------------------------------------------------------------------------
// 2. Folding, the saved values and the trampoline order
// -----------------------------------------------------------------------------------------

/// `u32 f(this, u32)`: the two callbacks of a `pre` loop (Override then Supersede) fold to
/// Supersede, the value is the Supersede one's, the trampoline skips the original, the `post`
/// loop saves `original = false` too and `make_return` destroys the saved value exactly once.
/// A callback that re-enters the hooked function gets its callbacks run again: each dispatch
/// clones the callback list under the lock and releases the lock before any callback runs
/// (`khook.hpp:733-739`), so a nested dispatch neither deadlocks nor works on a borrowed list.
/// Under a design that took the list out for the duration of a dispatch, the nested call would
/// find no callbacks and this would count 1.
#[test]
fn reentrant_dispatch_runs_the_callbacks_again() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let pre: Vec<h_u32::PreCb> = vec![Arc::new({
            let calls = Arc::clone(&calls);
            move |_call: &mut HookCall, this: *mut c_void, a: u32| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    // The callback itself reaches the hooked function: KHook's trampoline
                    // would run this hook's thunks again for that call.
                    // SAFETY: the thunks are called exactly as the trampoline calls them.
                    let _ = unsafe { h_u32::pre(this, a) };
                    let _ = unsafe { h_u32::post(this, a) };
                }
                Return::supersede_with(3u32)
            }
        })];
        let _hook = unsafe {
            h_u32::setup_inline(hooked_u32 as *mut c_void, pre, Vec::new(), None, SetupMode::Async)
        }
        .expect("the mock accepts the registration");

        mock.enter(hooked_u32 as *mut c_void);
        let _ = unsafe { h_u32::pre(fake_this(), 5) };
        mock.skip_original();
        let _ = unsafe { h_u32::post(fake_this(), 5) };
        let out = unsafe { h_u32::make_return(fake_this(), 5) };
        mock.leave();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the outer dispatch and the one the callback re-entered both ran the callback"
        );
        assert_eq!(out, 3, "the superseded value survived the nested dispatch");
    });
}

/// `u32 f(this, u32)`: the two callbacks of a `pre` loop (Override then Supersede) fold to
/// Supersede, the value is the Supersede one's, the trampoline skips the original, the `post`
/// loop saves `original = false` too and `make_return` destroys the saved value exactly once.
#[test]
fn u32_supersede_folding_skips_original() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a + 100
        }

        let pre: Vec<h_u32::PreCb> = vec![
            Arc::new(|_call: &mut HookCall, _this: *mut c_void, _a: u32| {
                Return::override_with(7u32)
            }),
            Arc::new(|_call: &mut HookCall, _this: *mut c_void, _a: u32| {
                Return::supersede_with(9u32)
            }),
        ];
        let post: Vec<h_u32::PostCb> = vec![Arc::new(
            |call: &mut HookCall, _this: *mut c_void, _a: u32| {
                // The trampoline recorded CompleteSkipped because the pre loop superseded
                // (`detour.cpp:889`), and no original value was saved.
                assert!(call.was_original_function_skipped());
                assert!(call.original_value_ptr().is_null());
                // The override value the fold saved, and the value the caller would get
                // right now (`GetCurrentValuePtr(false)`).
                let v = unsafe { *call.override_value_ptr().cast::<u32>() };
                assert_eq!(v, 9);
                assert_eq!(unsafe { *call.current_value_ptr().cast::<u32>() }, 9);
                Return::ignore(1u32)
            },
        )];

        let hook = unsafe {
            h_u32::setup_inline(hooked_u32 as *mut c_void, pre, post, None, SetupMode::Async)
        }
        .expect("the mock accepts the registration");
        assert_ne!(
            hook.id(),
            hook::INVALID_HOOK,
            "KHook answered a live id (the counter is process-wide, `detour.cpp:1548`)"
        );

        mock.enter(hooked_u32 as *mut c_void);
        let _pre_dummy = unsafe { h_u32::pre(fake_this(), 5) };
        // The trampoline: Supersede means no original call (`detour.cpp:886-915`).
        assert_eq!(mock.current_action(), Action::Supersede);
        mock.skip_original();
        let _post_dummy = unsafe { h_u32::post(fake_this(), 5) };
        let out = unsafe { h_u32::make_return(fake_this(), 5) };
        mock.leave();

        assert_eq!(out, 9, "the Supersede callback's value is what the caller gets");

        let saves = mock.saves();
        assert_eq!(saves.len(), 2, "one save from pre, one from post");
        // The `pre` save: the folded action and the *Supersede* callback's value (9, not 7).
        assert_eq!(saves[0].action, Action::Supersede);
        assert!(!saves[0].original, "pre/post save with original = false");
        assert_eq!(saves[0].size, 4);
        assert_eq!(saves[0].bytes, 9u32.to_ne_bytes());
        // The `post` save: its single Ignore callback does not lower the call's action, so it
        // asks for nothing and hands back the dummy.
        assert_eq!(saves[1].action, Action::Ignore);
        assert!(!saves[1].original, "pre/post save with original = false");
        assert_eq!(saves[1].size, 4);

        assert_eq!(mock.destroys(), 1, "make_return destroys the value exactly once");
        // The original was never called: GetOriginalFunction stays untouched.
        assert!(!mock
            .calls()
            .iter()
            .any(|c| matches!(c, Call::GetOriginalFunction)));
    });
}

/// `u32 f(this, u32)`: an `Override` pre keeps the original call; `make_call_original` saves
/// its result as the original value (`original = true`), which the `post` callback can read,
/// and a `post` `Supersede` (strictly above the pre's `Override`) is what the caller gets.
#[test]
fn u32_override_calls_original_and_post_supersedes() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a + 100
        }

        let pre: Vec<h_u32::PreCb> = vec![Arc::new(
            |_call: &mut HookCall, _this: *mut c_void, _a: u32| Return::override_with(7u32),
        )];
        let post: Vec<h_u32::PostCb> = vec![Arc::new(
            |call: &mut HookCall, _this: *mut c_void, _a: u32| {
                assert!(
                    !call.was_original_function_skipped(),
                    "an Override still calls the original"
                );
                let original = unsafe { *call.original_value_ptr().cast::<u32>() };
                assert_eq!(original, 105, "the original ran with a = 5");
                supersede_in_post(call)
            },
        )];

        fn supersede_in_post(call: &mut HookCall) -> Return<u32> {
            // `GetOverrideValuePtr` holds the pre callback's value at this point, and
            // `GetOriginalFunction` still names the unhooked function.
            assert_eq!(unsafe { *call.override_value_ptr().cast::<u32>() }, 7);
            assert_eq!(call.original_function(), hooked_u32 as *mut c_void);
            Return::supersede_with(42u32)
        }

        let hook = unsafe {
            h_u32::setup_inline(hooked_u32 as *mut c_void, pre, post, None, SetupMode::Async)
        }
        .expect("the mock accepts the registration");
        assert_ne!(
            hook.id(),
            hook::INVALID_HOOK,
            "KHook answered a live id (the counter is process-wide, `detour.cpp:1548`)"
        );

        mock.enter(hooked_u32 as *mut c_void);
        let _ = unsafe { h_u32::pre(fake_this(), 5) };
        assert_eq!(mock.current_action(), Action::Override);
        mock.begin_original_call();
        let orig = unsafe { h_u32::make_call_original(fake_this(), 5) };
        mock.end_original_call();
        let _ = unsafe { h_u32::post(fake_this(), 5) };
        let out = unsafe { h_u32::make_return(fake_this(), 5) };
        mock.leave();

        assert_eq!(orig, 105, "make_call_original returns the original's value");
        assert_eq!(out, 42, "the post callback's Supersede wins");

        let saves = mock.saves();
        assert_eq!(saves.len(), 3, "pre, call-original, post");
        let original_save = saves
            .iter()
            .find(|s| s.original)
            .expect("make_call_original saves with original = true");
        assert_eq!(original_save.action, Action::Ignore);
        assert_eq!(original_save.size, 4);
        assert_eq!(original_save.bytes, 105u32.to_ne_bytes());
        assert!(!saves[0].original, "pre saves with original = false");
        assert!(!saves[2].original, "post saves with original = false");
        assert_eq!(saves[2].action, Action::Supersede);
        assert_eq!(saves[2].bytes, 42u32.to_ne_bytes());
        assert_eq!(mock.destroys(), 1);
    });
}

/// The other half of `detour.cpp:383`: an action that is *not* strictly greater than the one
/// already saved does not replace the value, so a `post` `Override` at the same level as a
/// `pre` `Override` leaves the pre's value in place — the caller gets it.
#[test]
fn post_override_at_the_same_level_does_not_replace() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a + 100
        }

        let pre: Vec<h_u32::PreCb> = vec![Arc::new(
            |_call: &mut HookCall, _this: *mut c_void, _a: u32| Return::override_with(7u32),
        )];
        let post: Vec<h_u32::PostCb> = vec![Arc::new(
            |_call: &mut HookCall, _this: *mut c_void, _a: u32| Return::override_with(42u32),
        )];
        let _hook = unsafe {
            h_u32::setup_inline(hooked_u32 as *mut c_void, pre, post, None, SetupMode::Async)
        }
        .expect("the mock accepts the registration");

        mock.enter(hooked_u32 as *mut c_void);
        let _ = unsafe { h_u32::pre(fake_this(), 5) };
        mock.begin_original_call();
        let _ = unsafe { h_u32::make_call_original(fake_this(), 5) };
        mock.end_original_call();
        let _ = unsafe { h_u32::post(fake_this(), 5) };
        let out = unsafe { h_u32::make_return(fake_this(), 5) };
        mock.leave();

        assert_eq!(
            out, 7,
            "Override is not > Override, so the value saved first stands (detour.cpp:383)"
        );
        let saves = mock.saves();
        assert_eq!(saves[2].action, Action::Override);
        assert_eq!(saves[2].bytes, 42u32.to_ne_bytes(), "the post still asked for 42");
    });
}

/// `void f(this)`: the whole chain still runs with a zero-sized return type, and its
/// `SaveReturnValue` calls are the header's `null, 0, null, null` shape
/// (`khook.hpp:335-338`).
#[test]
fn void_hook_runs_and_saves_zero_size() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_void(_this: *mut c_void) {}

        let pre: Vec<h_void::PreCb> = vec![Arc::new(
            |_call: &mut HookCall, _this: *mut c_void| Return::supersede_with(()),
        )];
        let hook = unsafe { h_void::setup_inline(hooked_void as *mut c_void, pre, Vec::new(), None, SetupMode::Async) }
            .expect("the mock accepts the registration");

        mock.enter(hooked_void as *mut c_void);
        let _: () = unsafe { h_void::pre(fake_this()) };
        assert_eq!(mock.current_action(), Action::Supersede);
        mock.skip_original();
        let _: () = unsafe { h_void::post(fake_this()) };
        let out: () = unsafe { h_void::make_return(fake_this()) };
        mock.leave();

        assert_eq!(out, ());
        let saves = mock.saves();
        assert_eq!(saves.len(), 2);
        for save in &saves {
            assert_eq!(save.size, 0, "a void return saves nothing");
            assert!(save.ptr_is_null, "khook.hpp:335-338 passes a null pointer");
            assert!(!save.original);
        }
        assert_eq!(saves[0].action, Action::Supersede);
        assert_eq!(mock.destroys(), 1);
        assert_ne!(
            hook.id(),
            hook::INVALID_HOOK,
            "KHook answered a live id (the counter is process-wide, `detour.cpp:1548`)"
        );
    });
}

/// `V3 f(this, i32, *mut c_void)`: a 12-byte `#[repr(C)]` struct return, byte-exact through
/// the original call, the override and `make_return`.
///
/// On Itanium a 12-byte trivially-copyable struct comes back in `XMM0:XMM1` — three floats are
/// SSE class — so this signature uses the plain shape (`HIDDEN_RETURN_BUFFER == false`); the
/// MSVC member ABI needs the hidden pointer after `this`, which the `sret_msvc = sret` flag
/// compiles for on that target (checked by the windows build, not runnable here).
#[cfg(not(windows))]
#[test]
fn v3_struct_return_is_byte_exact() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_v3(_this: *mut c_void, a: i32, _p: *mut c_void) -> V3 {
            V3 {
                x: a as f32,
                y: 9.0,
                z: 1.5,
            }
        }

        let original_v3 = V3 {
            x: 3.0,
            y: 9.0,
            z: 1.5,
        };
        let superseded = V3 {
            x: 4.0,
            y: 5.0,
            z: 6.0,
        };

        let pre: Vec<h_v3::PreCb> = vec![Arc::new(
            |_call: &mut HookCall, _this: *mut c_void, _a: i32, _p: *mut c_void| {
                Return::override_with(V3 {
                    x: 7.0,
                    y: 7.0,
                    z: 7.0,
                })
            },
        )];
        let post: Vec<h_v3::PostCb> = vec![Arc::new(move |
            call: &mut HookCall,
            _this: *mut c_void,
            _a: i32,
            _p: *mut c_void,
        | {
            // The original's 12 bytes are readable through the context object.
            let seen = unsafe { *call.original_value_ptr().cast::<V3>() };
            assert_eq!(seen, original_v3);
            Return::supersede_with(superseded)
        })];
        let _hook = unsafe {
            h_v3::setup_inline(hooked_v3 as *mut c_void, pre, post, None, SetupMode::Async)
        }
        .expect("the mock accepts the registration");

        mock.enter(hooked_v3 as *mut c_void);
        let _ = unsafe { h_v3::pre(fake_this(), 3, core::ptr::null_mut()) };
        mock.begin_original_call();
        let orig = unsafe { h_v3::make_call_original(fake_this(), 3, core::ptr::null_mut()) };
        mock.end_original_call();
        let _ = unsafe { h_v3::post(fake_this(), 3, core::ptr::null_mut()) };
        let out = unsafe { h_v3::make_return(fake_this(), 3, core::ptr::null_mut()) };
        mock.leave();

        assert_eq!(orig, original_v3, "the original's 12 bytes survive the call");
        assert_eq!(out, superseded, "make_return hands back the superseded value");

        let saves = mock.saves();
        assert_eq!(saves.len(), 3);
        let original_save = saves.iter().find(|s| s.original).expect("original save");
        assert_eq!(original_save.size, 12);
        assert_eq!(original_save.bytes, as_bytes(original_v3));
        let last = saves.last().expect("post save");
        assert_eq!(last.action, Action::Supersede);
        assert_eq!(last.size, 12);
        assert_eq!(last.bytes, as_bytes(superseded));
        assert_eq!(
            mock.destroys(),
            1,
            "DestroyReturnValue runs once, after the value was read"
        );
    });
}

/// The same 12 bytes as a byte vector, for comparing what the mock copied.
#[cfg(not(windows))]
fn as_bytes(v: V3) -> Vec<u8> {
    // SAFETY: `V3` is `#[repr(C)]` and `Copy`, so a 12-byte read at its address is its bytes.
    unsafe { core::slice::from_raw_parts(core::ptr::from_ref(&v).cast::<u8>(), 12).to_vec() }
}

/// `CSplitScreenSlot f(this, i32)`: a **4-byte class** return on the Itanium shape — no hidden
/// pointer, the value travels in `eax` — byte-exact through the original call, the override and
/// `make_return`. The MSVC half of the same row is the hidden-pointer-after-`this` shape
/// (`sret_msvc = sret`), compile-checked by the windows build; a 4-byte class is exactly the
/// size the old doc wrongly claimed came back in `RAX` on that ABI.
#[cfg(not(windows))]
#[test]
fn four_byte_class_return_is_byte_exact() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_slot4(_this: *mut c_void, a: i32) -> CSplitScreenSlot {
            CSplitScreenSlot { m_data: a + 1 }
        }

        let pre: Vec<h_slot4::PreCb> = vec![Arc::new(
            |_call: &mut HookCall, _this: *mut c_void, _a: i32| {
                Return::override_with(CSplitScreenSlot { m_data: 11 })
            },
        )];
        let post: Vec<h_slot4::PostCb> = vec![Arc::new(|call: &mut HookCall, _this: *mut c_void, _a: i32| {
            // The original's 4 bytes are readable through the context object.
            assert_eq!(
                unsafe { *call.original_value_ptr().cast::<CSplitScreenSlot>() },
                CSplitScreenSlot { m_data: 8 }
            );
            Return::supersede_with(CSplitScreenSlot { m_data: 42 })
        })];
        let _hook = unsafe {
            h_slot4::setup_inline(hooked_slot4 as *mut c_void, pre, post, None, SetupMode::Async)
        }
        .expect("the mock accepts the registration");

        mock.enter(hooked_slot4 as *mut c_void);
        let _ = unsafe { h_slot4::pre(fake_this(), 7) };
        mock.begin_original_call();
        let orig = unsafe { h_slot4::make_call_original(fake_this(), 7) };
        mock.end_original_call();
        let _ = unsafe { h_slot4::post(fake_this(), 7) };
        let out = unsafe { h_slot4::make_return(fake_this(), 7) };
        mock.leave();

        assert_eq!(orig.m_data, 8, "the original's 4 bytes survive the call");
        assert_eq!(out.m_data, 42, "make_return hands back the superseded value");

        let saves = mock.saves();
        assert_eq!(saves.len(), 3, "pre, call-original, post");
        let original_save = saves.iter().find(|s| s.original).expect("original save");
        assert_eq!(original_save.size, 4);
        assert_eq!(
            original_save.bytes,
            8i32.to_ne_bytes(),
            "a 4-byte class is one machine slot"
        );
        let last = saves.last().expect("post save");
        assert_eq!(last.action, Action::Supersede);
        assert_eq!(last.size, 4);
        assert_eq!(last.bytes, 42i32.to_ne_bytes());
        assert_eq!(mock.destroys(), 1);
    });
}

/// `V24 f(this, i32)`: a 24-byte struct return, which both ABIs pass through a hidden pointer,
/// so this is the Itanium sret shape (`ret` before `this`) on Linux. Both the original call
/// and `make_return` write through the caller's buffer.
#[cfg(not(windows))]
#[test]
fn v24_sret_shape_writes_through_the_buffer() {
    with_mock(|mock| {
        /// The hooked function in the Itanium sret shape: the caller's return buffer comes
        /// first and is handed back.
        unsafe extern "C" fn hooked_v24(
            ret: *mut V24,
            _this: *mut c_void,
            a: i32,
        ) -> *mut V24 {
            // SAFETY: the caller passed a live `V24` buffer.
            unsafe {
                *ret = V24 {
                    w: [a as u64, 8, 9],
                }
            };
            ret
        }

        let original_v24 = V24 { w: [3, 8, 9] };
        let superseded = V24 {
            w: [11, 12, 13],
        };

        let pre: Vec<h_v24::PreCb> = vec![Arc::new(
            |_call: &mut HookCall,
             _this: *mut c_void,
             _a: i32| { Return::override_with(V24 { w: [1, 1, 1] }) },
        )];
        let post: Vec<h_v24::PostCb> = vec![Arc::new(move |
            call: &mut HookCall,
            _this: *mut c_void,
            _a: i32,
        | {
            let seen = unsafe { *call.original_value_ptr().cast::<V24>() };
            assert_eq!(seen, original_v24);
            Return::supersede_with(superseded)
        })];
        let _hook = unsafe {
            h_v24::setup_inline(hooked_v24 as *mut c_void, pre, post, None, SetupMode::Async)
        }
        .expect("the mock accepts the registration");

        let mut buffer = V24::default();
        mock.enter(hooked_v24 as *mut c_void);
        let _ = unsafe { h_v24::pre(&mut buffer, fake_this(), 3) };
        mock.begin_original_call();
        let orig = unsafe { h_v24::make_call_original(&mut buffer, fake_this(), 3) };
        mock.end_original_call();
        assert_eq!(unsafe { *orig }, original_v24, "the original wrote the buffer");
        let _ = unsafe { h_v24::post(&mut buffer, fake_this(), 3) };
        let out = unsafe { h_v24::make_return(&mut buffer, fake_this(), 3) };
        mock.leave();

        assert!(core::ptr::eq(out, &buffer), "the buffer is returned");
        assert_eq!(
            unsafe { *out },
            superseded,
            "make_return copied the superseded value through the buffer"
        );

        let saves = mock.saves();
        let original_save = saves.iter().find(|s| s.original).expect("original save");
        assert_eq!(original_save.size, 24);
        assert_eq!(original_save.bytes, unsafe {
            core::slice::from_raw_parts(core::ptr::from_ref(&original_v24).cast::<u8>(), 24)
                .to_vec()
        });
        let last = saves.last().expect("post save");
        assert_eq!(last.action, Action::Supersede);
        assert_eq!(last.bytes.len(), 24);
        assert_eq!(mock.destroys(), 1);
    });
}

// -----------------------------------------------------------------------------------------
// 3. Ownership: RemoveHook on drop, once, with the id it registered
// -----------------------------------------------------------------------------------------

/// `Drop` asks KHook for the **asynchronous** removal (`khook.hpp:673`, "For safety reasons"),
/// so the removal function runs later, on KHook's delete thread, and the context outlives the
/// [`Hook`]: it must stay alive for a deferred callback and be freed exactly once, by the
/// removal function. This is also where the mock models the delete thread, and where
/// [`hook::live_context_count`] is lowered.
#[test]
fn dropping_the_hook_removes_it_asynchronously_and_frees_it_on_the_delete_thread() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a
        }

        let live = Arc::new(());
        let weak = Arc::downgrade(&live);
        let pre: Vec<h_u32::PreCb> = vec![Arc::new({
            let live = Arc::clone(&live);
            move |_call: &mut HookCall, _this: *mut c_void, _a: u32| {
                let _ = &live;
                Return::ignore(0u32)
            }
        })];
        drop(live);
        assert!(
            weak.upgrade().is_some(),
            "the context's callback holds the probe"
        );

        let hook_id = {
            let hook = unsafe {
                h_u32::setup_inline(hooked_u32 as *mut c_void, pre, Vec::new(), None, SetupMode::Async)
            }
            .expect("the mock accepts the registration");
            assert_ne!(
                hook.id(),
                hook::INVALID_HOOK,
                "KHook answered a live id (the counter is process-wide, `detour.cpp:1548`)"
            );
            assert_eq!(
                hook::live_context_count(),
                1,
                "a registered hook is a context KHook can call into"
            );
            assert!(
                mock.removal_calls().is_empty(),
                "nothing removed the hook yet"
            );
            hook.id()
            // `hook` drops here: RemoveHook (async), then our reference, and the removal
            // function is left to KHook's delete thread.
        };
        assert_ne!(hook_id, hook::INVALID_HOOK, "KHook answered a live id");
        assert_eq!(
            mock.removal_calls(),
            Vec::new(),
            "the asynchronous removal has not run yet"
        );
        assert!(
            weak.upgrade().is_some(),
            "KHook still holds the context, so a late callback can still run"
        );
        assert_eq!(
            hook::live_context_count(),
            1,
            "KHook can still call into the context until its removal function runs"
        );

        let remove_calls: Vec<Call> = mock
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::RemoveHook { .. }))
            .collect();
        assert_eq!(remove_calls.len(), 1, "Drop removes the hook exactly once");
        match &remove_calls[0] {
            Call::RemoveHook {
                id,
                r#async,
                hook_removal_fn,
                context,
            } => {
                assert_eq!(*id, hook_id, "with the id KHook registered");
                assert!(
                    *r#async,
                    "asynchronously, like the C++ templates (khook.hpp:673)"
                );
                assert_eq!(*hook_removal_fn, 0, "KHook calls the detour's own remove fn");
                assert_eq!(*context, 0);
            }
            other => panic!("unexpected call {other:?}"),
        }

        // The delete thread runs it: the removal function, with the registering context pushed,
        // and the context — which the `Hook` can no longer reach — is freed by the last release.
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(
            mock.removal_calls(),
            vec![hook_id],
            "KHook's removal function ran exactly once, with the context pushed"
        );
        assert!(
            weak.upgrade().is_none(),
            "the context was freed exactly once: neither leaked nor kept alive"
        );
        assert_eq!(hook::live_context_count(), 0, "KHook is done with it");

        // The registration passed the thunks and the computed stack size through.
        match mock.calls().first().expect("a SetupHook call") {
            Call::SetupHook {
                function,
                stack_size,
                r#async,
                ..
            } => {
                assert_eq!(*function, hooked_u32 as *mut c_void as usize);
                assert_eq!(*stack_size, h_u32::STACK_SIZE);
                assert!(*r#async, "installed asynchronously by default");
            }
            other => panic!("unexpected call {other:?}"),
        }
    });
}

/// The other side of the same decision: `SetupMode::Sync` is the opt-in that reaches KHook as
/// `async = false`. It asks for a synchronous insert; KHook inserts the first hook on a fresh
/// address synchronously either way, and queues the work when it cannot take the capsule's
/// mutex (`detour.cpp:1712-1717,1738-1748`), so nothing here promises the hook is live on
/// return.
#[test]
fn setup_mode_sync_installs_with_async_false() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a
        }

        let hook = unsafe {
            h_u32::setup_inline(
                hooked_u32 as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Sync,
            )
        }
        .expect("the mock accepts the registration");
        assert_eq!(SetupMode::default(), SetupMode::Async);
        assert!(!SetupMode::Sync.is_async());
        assert!(SetupMode::Async.is_async());
        match mock.calls().first().expect("a SetupHook call") {
            Call::SetupHook { r#async, .. } => {
                assert!(!*r#async, "the opt-in reaches KHook as async = false");
            }
            other => panic!("unexpected call {other:?}"),
        }
        drop(hook);
        // The removal is asynchronous either way (khook.hpp:673).
        match mock
            .calls()
            .iter()
            .find(|c| matches!(c, Call::RemoveHook { .. }))
            .expect("Drop asks for a removal")
        {
            Call::RemoveHook { r#async, .. } => assert!(*r#async),
            other => panic!("unexpected call {other:?}"),
        }
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(hook::live_context_count(), 0);
    });
}

// -----------------------------------------------------------------------------------------
// 3b. Unload safety: the live-context count, remove_all_blocking, and the delete thread
// -----------------------------------------------------------------------------------------

/// A **positive** `remove_all_blocking`: one hook is still registered (it is removed
/// synchronously, `async = false`: `detour.cpp:1870-1876` runs the removal function before
/// `RemoveHook` returns) and another was dropped earlier, whose asynchronous removal KHook's
/// delete thread has queued. That thread runs here as a real thread, racing the re-issue
/// (`__RemoveHook_Sync` holds `associated_hooks_mutex` exclusively, which the mock models with
/// [`Mock::claim_and_run_removal`]), so whichever of the two gets there first, each removal
/// function runs exactly once and the drain reports `Ok`; only the still-registered hook needs
/// re-issuing. Contrast with
/// [`remove_all_blocking_reissues_a_queued_removal_without_the_delete_thread`], where that
/// thread never runs at all.
#[test]
fn remove_all_blocking_and_the_delete_thread_cannot_double_run_a_removal() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a ^ 0x71
        }
        unsafe extern "C" fn hooked_void(_this: *mut c_void) {}

        let dropped = unsafe {
            h_u32::setup_inline(
                hooked_u32 as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        let still_registered = unsafe {
            h_void::setup_inline(
                hooked_void as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        let (dropped_id, still_id) = (dropped.id(), still_registered.id());
        assert!(dropped_id < still_id, "the counter is process-wide and monotonic");
        assert_eq!(hook::live_context_count(), 2);

        // One hot-unplugged hook: its removal is now queued for the delete thread.
        drop(dropped);
        assert_eq!(mock.deferred_len(), 1, "the async removal is queued");

        // The mock's delete thread, as a real thread: it waits for queued removals and runs
        // them one at a time (KHook's own loop, `detour.cpp:1630-1669`).
        let delete_thread = std::thread::spawn(move || {
            for _ in 0..1 {
                mock.fire_deferred_removal(Duration::from_secs(10));
            }
        });

        let out = hook::remove_all_blocking(Duration::from_secs(10));
        assert!(
            out.is_ok(),
            "both removals ran, whichever way round: {out:?}"
        );
        delete_thread.join().expect("the delete thread finished");
        assert_eq!(hook::live_context_count(), 0, "KHook is done with both");
        assert!(
            mock.associated().is_empty(),
            "KHook no longer holds either hook"
        );
        assert!(
            mock.skipped_removals().len() <= 1,
            "at most one of the two removal paths found the queued hook already gone: {:?}",
            mock.skipped_removals()
        );

        let mut removed = mock.removal_calls();
        removed.sort_unstable();
        assert_eq!(
            removed,
            vec![dropped_id, still_id],
            "each removal function ran exactly once, for its own hook"
        );

        // `remove_all_blocking` re-issued the still-registered hook (order: what was left
        // installed first, then what was still pending) and did not touch the one the `Drop` had
        // already queued — the mock's delete thread issues no `RemoveHook` of its own.
        assert_eq!(
            mock.removals(),
            vec![(dropped_id, true), (still_id, false), (dropped_id, false)],
            "Drop queued the dropped hook; the drain re-issued both halves of what KHook had left"
        );

        // A `Hook` that outlives the drain is harmless: KHook no longer has the id, so the
        // `Drop` asks for a removal that runs nothing and is not re-issued afterwards either.
        drop(still_registered);
        assert_eq!(
            mock.removal_calls().len(),
            2,
            "no second removal function ran for the drained hook"
        );
        let after_drop = mock.calls().len();
        assert!(hook::remove_all_blocking(Duration::ZERO).is_ok());
        assert_eq!(
            mock.calls().len(),
            after_drop,
            "a removal that already ran is not re-issued"
        );
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// The delete thread of KHook may be gone or behind while a queued removal still has *this
/// module's* removal function to run — `KHook::Shutdown` joins the thread
/// (`detour.cpp:1879-1895`) and a hook still waiting in `insert_hooks` is removed by a scan
/// rather than by it. `remove_all_blocking` must therefore not wait for that thread: it re-issues
/// the removal synchronously (`async = false`), which returns only once the removal function has
/// retired, so this finishes long before the deadline and without the thread ever running.
#[test]
fn remove_all_blocking_reissues_a_queued_removal_without_the_delete_thread() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a ^ 0x72
        }

        let hook = unsafe {
            h_u32::setup_inline(
                hooked_u32 as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        let hook_id = hook.id();
        drop(hook);
        assert_eq!(mock.deferred_len(), 1, "Drop queued the asynchronous removal");
        assert_eq!(
            hook::live_context_count(),
            1,
            "KHook can still call into the context until its removal function runs"
        );
        assert_eq!(mock.removal_calls(), Vec::new(), "nothing ran yet");

        // No delete thread at all, and a deadline far longer than this should ever need.
        let deadline = Duration::from_millis(500);
        let start = Instant::now();
        let out = hook::remove_all_blocking(deadline);
        let waited = start.elapsed();
        assert!(
            out.is_ok(),
            "the re-issued removal ran synchronously: {out:?}"
        );
        println!("re-issued removal returned after {waited:?}");
        assert!(
            waited < deadline / 2,
            "it did not wait for the delete thread ({waited:?})"
        );
        assert_eq!(
            mock.removal_calls(),
            vec![hook_id],
            "the removal function ran once"
        );
        assert_eq!(hook::live_context_count(), 0, "KHook is done with it");
        assert!(mock.associated().is_empty());
        assert!(
            mock.skipped_removals().is_empty(),
            "the mock's delete thread never ran"
        );
        assert_eq!(
            mock.deferred_len(),
            1,
            "the queued removal is still in KHook's queue; nothing needed it"
        );

        let removals: Vec<(HookId, bool)> = mock
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::RemoveHook { id, r#async, .. } if id != hook::INVALID_HOOK => {
                    Some((id, r#async))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            removals,
            vec![(hook_id, true), (hook_id, false)],
            "Drop queued it asynchronously, the drain re-issued it synchronously"
        );
        assert_eq!(
            mock.barriers(),
            1,
            "and the drain closed with the barrier that waits for that removal's epilogue"
        );
    });
}

/// The other half of the `pending` bookkeeping: a hook whose removal function has already run —
/// here the mock's delete thread got to it — is no longer in the set of removals KHook has to run
/// ([`s2sdk::hook::remove_all_blocking`]'s removal function removes its own id), so a later drain
/// neither re-issues it nor runs a second removal function for it.
#[test]
fn remove_all_blocking_does_not_reissue_a_removal_that_already_ran() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a ^ 0x73
        }

        let hook = unsafe {
            h_u32::setup_inline(
                hooked_u32 as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        let hook_id = hook.id();
        drop(hook);
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(mock.removal_calls(), vec![hook_id]);
        assert_eq!(hook::live_context_count(), 0);

        let before = mock.removals();
        let out = hook::remove_all_blocking(Duration::from_millis(500));
        assert!(out.is_ok(), "there is nothing left to remove: {out:?}");
        assert_eq!(
            mock.removals(),
            before,
            "only `Drop`'s queued removal was ever issued: the id had already left the pending set"
        );
        assert_eq!(
            mock.barriers(),
            1,
            "but the drain still closes with its barrier, because a removal function *has* run \
             and its epilogue was the only thing left to wait for"
        );
        assert_eq!(
            mock.removal_calls(),
            vec![hook_id],
            "no second removal function ran for it"
        );
    });
}

/// The barrier: a removal function of this module lowers the count *inside* itself, so the drain
/// can see zero while the function's signal, its return and KHook's release of its own bookkeeping
/// mutexes are still ahead of it on KHook's thread. The drain must not report `Ok` then — the mock
/// parks a removal in exactly that window while holding its removal gate (what `__RemoveHook_Sync`
/// holds `associated_hooks_mutex` exclusively for, `detour.cpp:1581-1597`), and this test asserts
/// the drain arrived at the gate and had to wait for it, deterministically (no timing assumption).
#[test]
fn remove_all_blocking_waits_out_the_removal_functions_epilogue() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a ^ 0x75
        }

        let hook = unsafe {
            h_u32::setup_inline(
                hooked_u32 as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        let hook_id = hook.id();
        drop(hook);
        assert_eq!(mock.deferred_len(), 1);

        // KHook's delete thread runs the removal and stops between our removal function returning
        // and KHook getting back to its loop.
        mock.park_after_removal();
        let delete_thread = std::thread::spawn(move || {
            mock.fire_deferred_removal(Duration::from_secs(10));
        });
        mock.wait_for_parked_removal(Duration::from_secs(10));
        assert_eq!(
            hook::live_context_count(),
            0,
            "the removal function lowered the count before returning"
        );
        assert_eq!(mock.removal_calls(), vec![hook_id]);
        assert!(!mock
            .sync_removal_met_parked_epilogue
            .load(Ordering::SeqCst));

        // The drain has nothing left to remove and sees a zero count, so only the barrier can tell
        // it that the removal function is really over: it has to block on the gate the parked
        // removal is still holding.
        let drain = std::thread::spawn(move || hook::remove_all_blocking(Duration::from_millis(500)));
        mock.wait_for_sync_removal_at_the_parked_epilogue(Duration::from_secs(10));
        assert_eq!(
            mock.barriers(),
            1,
            "the drain's barrier is what arrived, while the removal function was still on \
             KHook's stack"
        );
        assert_eq!(
            mock.removals(),
            vec![(hook_id, true)],
            "and it re-issued nothing: the hook was already removed"
        );

        // Nothing of this module may be left running once the drain reports `Ok`, so it can only
        // get there after the removal it was waiting for is over.
        mock.release_parked_removal();
        let out = drain.join().expect("the drain finished");
        assert!(out.is_ok(), "the drain reported Ok: {out:?}");
        delete_thread.join().expect("the delete thread finished");
    });
}

// -----------------------------------------------------------------------------------------
// 3d. Thunk-time refusals, and a `Drop` from a thunk that must not wait for the gate
// -----------------------------------------------------------------------------------------

/// A `Drop` from inside a hook thunk must not wait for [`KHOOK_GATE`]: the thread holding that
/// gate may be inside a synchronous `RemoveHook` spinning on *this* call's capsule (the hooked
/// call holds it shared, `detour.cpp:214,1503,1596`), so a gate wait here would leave the two
/// waiting for each other for ever.
///
/// The mock holds the removal of one hook until the callback thread has dropped another one, so
/// the gate is genuinely held across that drop. The rendezvous is a bounded wait; with the
/// in-thunk gate skip removed, the drop blocks on the gate and this test fails within five
/// seconds instead of hanging.
#[test]
fn a_drop_from_a_callback_does_not_wait_for_the_gate() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        unsafe extern "C" fn other(_this: *mut c_void) {}

        let (registered_tx, registered_rx) = std::sync::mpsc::channel::<HookId>();
        let (armed_tx, armed_rx) = std::sync::mpsc::channel::<()>();
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel::<()>();

        std::thread::scope(|scope| {
            // The "callback": its hook never leaves this thread (`Hook` is `!Send`), and it is
            // dropped here with the depth guard set, exactly as a `pre` callback that drops a
            // `Hook` does.
            let callback = scope.spawn(move || {
                let hook = unsafe {
                    h_void::setup_inline(
                        hooked as *mut c_void,
                        Vec::new(),
                        Vec::new(),
                        None,
                        SetupMode::Async,
                    )
                }
                .expect("registered on the callback's thread");
                registered_tx.send(hook.id()).unwrap();
                armed_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the synchronous removal never started");
                // Wait until the removing thread is really inside the mock, with the gate held.
                mock.wait_for_held_sync_removal(Duration::from_secs(5));
                let _depth = hook::ThunkGuard::enter();
                drop(hook);
                dropped_tx.send(()).unwrap();
            });

            // The remover: registers its own hook after the callback's, then removes it
            // synchronously; the mock stops that removal inside the call.
            let remover = scope.spawn(move || {
                registered_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the callback never registered");
                let hook = unsafe {
                    h_void::setup_inline(
                        other as *mut c_void,
                        Vec::new(),
                        Vec::new(),
                        None,
                        SetupMode::Async,
                    )
                }
                .expect("registered");
                mock.hold_sync_removal(hook.id());
                armed_tx.send(()).unwrap();
                hook.remove_sync()
            });

            dropped_rx.recv_timeout(Duration::from_secs(5)).expect(
                "the Drop blocked: it waited for KHOOK_GATE, which the synchronous removal on \
                 the other thread holds",
            );
            callback.join().expect("the callback finished");
            mock.release_held_sync_removal();
            let removal = remover.join().expect("the remover finished");
            assert_eq!(removal.expect("removed"), Removal::Removed);
        });
        assert_eq!(
            mock.deferred_len(),
            1,
            "the callback's Drop only queued its removal (khook.hpp:673)"
        );
        assert_eq!(
            hook::live_context_count(),
            1,
            "KHook still holds the callback's context until the delete thread runs it"
        );
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// A registration from inside a hook thunk with the gate free succeeds: from a thunk the
/// gate is taken with `try_lock` (a gate holder may be spinning in a synchronous removal on
/// this call's capsule, so waiting there could deadlock), and a registration's own KHook
/// calls never wait on a capsule — its capsule insert is a `try_lock` (`detour.cpp:1453`), so
/// a hook on the thread's own target just goes to KHook's insert thread. The hook is live and
/// callable inside the same thunk (what a GameFrame tick does), and it is removable later
/// from outside.
#[test]
fn registering_from_a_callback_with_the_gate_free_is_ok() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}

        let ran = Arc::new(AtomicUsize::new(0));
        let pre: Vec<h_void::PreCb> = vec![Arc::new({
            let ran = Arc::clone(&ran);
            move |_call: &mut HookCall, _this: *mut c_void| {
                ran.fetch_add(1, Ordering::SeqCst);
                Return::ignore(())
            }
        })];

        let _depth = hook::ThunkGuard::enter();
        let hook = unsafe {
            h_void::setup_inline(hooked as *mut c_void, pre, Vec::new(), None, SetupMode::Async)
        }
        .expect("a registration from a thunk with the gate free is ok");
        assert_ne!(hook.id(), hook::INVALID_HOOK);
        assert!(hook::in_thunk(), "still inside the thunk");
        assert!(mock.is_associated(hook.id()), "the hook is live in the mock");
        assert_eq!(hook::live_context_count(), 1, "the hook is counted live");
        assert!(
            mock.calls().iter().any(|c| matches!(c, Call::SetupHook { .. })),
            "the registration reached KHook: {:?}",
            mock.calls()
        );

        // Callable from inside the same thunk: the dispatch runs the new hook's callback, and
        // the trampoline's `make_return` finishes the call.
        mock.enter(hooked as *mut c_void);
        // SAFETY: the mock pushed the registering context and no return value is read for a
        // void signature.
        let _: () = unsafe { h_void::pre(fake_this()) };
        let _: () = unsafe { h_void::make_return(fake_this()) };
        mock.leave();
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the new hook's callback ran");
        drop(_depth);
        assert!(!hook::in_thunk());

        // Removable later, from outside the thunk.
        assert_eq!(hook.remove_sync().expect("removed"), Removal::Removed);
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// A registration from inside a hook thunk while another thread holds the gate answers
/// [`Error::GateBusy`] **immediately**: from a thunk the gate is taken with `try_lock`, never
/// with a blocking wait — the holder here is a synchronous removal parked inside the mock
/// (what `__RemoveHook_Sync` can be doing on a real capsule, `detour.cpp:1578-1598`), and
/// waiting for it could deadlock against this call's own capsule. Nothing was registered, no
/// KHook call was made, and the context was freed exactly as on every other error.
///
/// The rendezvous is a bounded wait; with the `try_lock` replaced by a blocking lock, the
/// registration never returns while the remover is parked and this test fails at the deadline
/// instead of passing.
#[test]
fn registering_from_a_callback_with_the_gate_held_is_gate_busy() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        unsafe extern "C" fn other(_this: *mut c_void) {}

        let (armed_tx, armed_rx) = std::sync::mpsc::channel::<()>();
        let (busy_tx, busy_rx) = std::sync::mpsc::channel::<()>();

        std::thread::scope(|scope| {
            // The "callback": it registers from inside a thunk while the remover's
            // synchronous removal is parked inside the mock, which holds KHOOK_GATE.
            let callback = scope.spawn(move || {
                armed_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the remover never armed its hold");
                // Wait until the remover is really inside the mock, with the gate held.
                mock.wait_for_held_sync_removal(Duration::from_secs(5));

                // The context the registration allocates owns the probe: a `GateBusy` releases
                // the caller's reference, so the probe goes away with it.
                let live = Arc::new(());
                let weak = Arc::downgrade(&live);
                let calls_before = mock.calls().len();
                let pre: Vec<h_void::PreCb> = vec![Arc::new({
                    let live = Arc::clone(&live);
                    move |_call: &mut HookCall, _this: *mut c_void| {
                        let _ = &live;
                        Return::ignore(())
                    }
                })];
                drop(live);

                let _depth = hook::ThunkGuard::enter();
                let out = unsafe {
                    h_void::setup_inline(
                        hooked as *mut c_void,
                        pre,
                        Vec::new(),
                        None,
                        SetupMode::Async,
                    )
                };
                drop(_depth);
                assert!(
                    matches!(out, Err(Error::GateBusy)),
                    "a registration from a thunk with the gate held must answer GateBusy, \
                     immediately: {out:?}"
                );
                assert_eq!(
                    mock.calls().len(),
                    calls_before,
                    "no KHook call was made: {:?}",
                    mock.calls()
                );
                assert!(
                    weak.upgrade().is_none(),
                    "the refused registration released the context"
                );
                busy_tx.send(()).unwrap();
            });

            // The remover: registers its own hook (the gate is free), then removes it
            // synchronously; the mock parks that removal inside the call, with KHOOK_GATE
            // still held by this thread — exactly the holder a thunk must never wait for.
            let remover = scope.spawn(move || {
                let hook = unsafe {
                    h_void::setup_inline(
                        other as *mut c_void,
                        Vec::new(),
                        Vec::new(),
                        None,
                        SetupMode::Async,
                    )
                }
                .expect("registered");
                mock.hold_sync_removal(hook.id());
                armed_tx.send(()).unwrap();
                hook.remove_sync()
            });

            // The bound: with a blocking lock where the try_lock belongs, the registration
            // never returns while the remover is parked, and this receive times out — the
            // failure mode, not a hang.
            busy_rx.recv_timeout(Duration::from_secs(5)).expect(
                "the registration blocked on KHOOK_GATE instead of try_locking it",
            );
            mock.release_held_sync_removal();
            let removal = remover.join().expect("the remover finished");
            assert_eq!(removal.expect("removed"), Removal::Removed);
            callback.join().expect("the callback finished");
        });
        assert_eq!(hook::live_context_count(), 0);
        assert!(
            mock.associated().is_empty(),
            "nothing of the case is left registered: {:?}",
            mock.associated()
        );
    });
}

/// A poisoned gate does not wedge an in-thunk registration: the `try_lock` the thunk path makes
/// recovers the poison exactly as a blocking gate take does (a poisoned but unlocked `try_lock`
/// answers [`std::sync::TryLockError::Poisoned`], whose inner guard the recovery takes — see
/// [`hook::Error::GateBusy`]). The poison is produced the way a real one is: a scoped thread
/// panics while the hook gate is held (its [`hook::Hook`]'s asynchronous removal panics inside
/// the mock), and after that an in-thunk registration succeeds and the hook is removed from
/// outside normally.
#[test]
fn a_poisoned_gate_recovers_for_the_try_path() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        unsafe extern "C" fn other(_this: *mut c_void) {}

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let hook = unsafe {
                    h_void::setup_inline(
                        other as *mut c_void,
                        Vec::new(),
                        Vec::new(),
                        None,
                        SetupMode::Async,
                    )
                }
                .expect("registered before the poisoning");
                mock.panic_async_remove.store(true, Ordering::SeqCst);
                // `drop` reaches the mock with KHOOK_GATE still held — `Hook::drop` waits for
                // the gate outside a thunk — and the panicking unwind poisons it.
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(hook)));
                assert!(
                    out.is_err(),
                    "the drop under the knob must panic, so the gate is poisoned"
                );
                mock.panic_async_remove.store(false, Ordering::SeqCst);
            });
        });
        // The gate is poisoned now and unlocked. An in-thunk registration takes it with
        // `khook_gate_try`, whose poisoned arm hands the guard out, so it registers as it would
        // with a fresh gate — not [`Error::GateBusy`], and no KHook call is skipped.
        let hook = {
            let _depth = hook::ThunkGuard::enter();
            unsafe {
                h_void::setup_inline(
                    hooked as *mut c_void,
                    Vec::new(),
                    Vec::new(),
                    None,
                    SetupMode::Async,
                )
            }
            .expect("the poisoned gate did not wedge the try path")
        };
        assert!(hook::live_context_count() > 0, "the registration landed");
        assert!(mock.is_associated(hook.id()));
        let id = hook.id();
        // And outside the thunk the blocking path recovers the poison the same way: the hook is
        // removed normally.
        assert_eq!(hook.remove_sync().expect("removed"), Removal::Removed);
        // The poisoned drop's context is still counted: nothing could run its removal once the
        // panic fired above.
        assert_eq!(hook::live_context_count(), 1);
        assert!(!mock.is_associated(id));

        // The `Drop` that panicked stranded its id in the pending set with its count up (no
        // removal could run for it once the knob panic fired above); the drain re-issues it
        // synchronously and closes it, exactly as [`remove_all_blocking`]'s contract reads.
        hook::remove_all_blocking(Duration::from_secs(10))
            .expect("the stranded pending id was drained");
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// `remove_sync` from inside a hook thunk hands the hook to the asynchronous path (every build):
/// it answers [`Removal::Deferred`] without spinning on
/// the capsule its own call holds shared, and the removal it queued is drained later by
/// [`remove_all_blocking`] from outside — no delete thread needed.
#[test]
fn remove_sync_from_a_callback_defers_and_the_drain_removes_it() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}

        let hook = unsafe {
            h_void::setup_inline(hooked as *mut c_void, Vec::new(), Vec::new(), None, SetupMode::Async)
        }
        .expect("registered");
        let id = hook.id();
        assert_eq!(hook::live_context_count(), 1);

        let removal = {
            let _depth = hook::ThunkGuard::enter();
            let out = hook.remove_sync();
            drop(_depth);
            out
        };
        assert_eq!(removal.expect("deferred"), Removal::Deferred);
        assert_eq!(
            hook::live_context_count(),
            1,
            "the context is still KHook's: the removal is only queued"
        );

        // From outside the thunk, the drain re-issues the queued removal synchronously.
        hook::remove_all_blocking(Duration::from_secs(10)).expect("drained");
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(mock.removal_calls(), vec![id]);
    });
}

/// `remove_all_blocking` from a thunk is refused with [`Error::InThunk`] (every build) before it
/// touches anything: its synchronous
/// removals would spin on the capsule this call holds shared (`detour.cpp:1503,1596`).
#[test]
fn remove_all_blocking_from_a_callback_is_refused() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked(_this: *mut c_void) {}
        // Something registered, so a call that got past the refusal would make KHook calls.
        let hook = unsafe {
            h_void::setup_inline(
                hooked as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("registered");
        let calls_before = mock.calls().len();
        let lost_before = hook::lost_removal_count();

        let out = {
            let _depth = hook::ThunkGuard::enter();
            hook::remove_all_blocking(Duration::from_secs(5))
        };
        assert!(
            matches!(out, Err(Error::InThunk)),
            "every build refuses a drain from a thunk with Error::InThunk"
        );
        assert_eq!(
            mock.calls().len(),
            calls_before,
            "nothing was asked of KHook: {:?}",
            mock.calls()
        );
        assert_eq!(hook::lost_removal_count(), lost_before);
        assert_eq!(hook::live_context_count(), 1, "the hook is still registered");
        assert!(mock.is_associated(hook.id()));
        drop(hook);
        mock.fire_deferred_removal(Duration::from_secs(10));
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// With nothing registered there is nothing to remove and nothing to wait for, so the call
/// succeeds without touching the API at all.
#[test]
fn remove_all_blocking_with_nothing_registered_is_ok() {
    with_mock(|mock| {
        let out = hook::remove_all_blocking(Duration::ZERO);
        assert!(out.is_ok(), "nothing to do: {out:?}");
        assert_eq!(hook::live_context_count(), 0);
        assert!(
            mock.calls().is_empty(),
            "no API call was needed: {:?}",
            mock.calls()
        );
    });
}

// -----------------------------------------------------------------------------------------
// 3c. One capsule per hooked address: its copy stack comes from the first registration
// -----------------------------------------------------------------------------------------

/// A registration on an address whose capsule was already created for a **smaller** stack is
/// refused: KHook hands it the existing `DetourCapsule` and ignores its own `stack_size`
/// (`detour.cpp:1696-1719`), so its trampoline would copy too little (`:794`). The refused
/// registration must leave nothing behind: no `SetupHook`, no context, no removal.
#[test]
fn capsule_stack_size_refuses_a_larger_later_registration_on_the_same_address() {
    with_mock(|mock| {
        // One address, two signatures: `u32 f(this, u32)` first (24 bytes of hook stack on
        // Linux, 32 on Win64, so a 32-byte capsule), then a five-argument void function that
        // needs 48.
        unsafe extern "C" fn hooked_shared(_this: *mut c_void, a: u32) -> u32 {
            a ^ 0x5A
        }

        let first = unsafe {
            h_u32::setup_inline(
                hooked_shared as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the first registration on the address creates the capsule");

        // The refused registration must free the context it built, like every other failure.
        let live = Arc::new(());
        let weak = Arc::downgrade(&live);
        let pre: Vec<h_void5::PreCb> = vec![Arc::new({
            let live = Arc::clone(&live);
            move |_call: &mut HookCall, _t: *mut c_void, _a: u64, _b: u64, _c: u64, _d: u64, _e: u64| {
                let _ = &live;
                Return::ignore(())
            }
        })];
        drop(live);

        let err = unsafe {
            h_void5::setup_inline(
                hooked_shared as *mut c_void,
                pre,
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect_err("48 bytes of hook stack do not fit the capsule the first one created");
        println!("capsule refusal: {err}");
        assert!(
            weak.upgrade().is_none(),
            "the refused registration freed its context"
        );
        let capsule = (h_u32::STACK_SIZE + 0xF) & !0xF;
        let needed = (h_void5::STACK_SIZE + 0xF) & !0xF;
        assert!(needed > capsule, "the case needs a bigger capsule size");
        match err {
            Error::CapsuleStackTooSmall {
                address,
                capsule: c,
                needed: n,
            } => {
                assert_eq!(address, hooked_shared as *mut c_void as usize);
                assert_eq!(c, capsule);
                assert_eq!(n, needed);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|c| matches!(c, Call::SetupHook { .. }))
                .count(),
            1,
            "the refused registration never reached KHook"
        );
        assert!(mock.removal_calls().is_empty());
        assert_eq!(hook::live_context_count(), 1, "only the first hook is KHook's");

        // The capsule is still the first one's: a registration that fits it is accepted.
        let fits = unsafe {
            h_u32::setup_inline(
                hooked_shared as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("a signature that fits the capsule is fine");
        assert_eq!(hook::live_context_count(), 2);
        drop(fits);
        drop(first);
    });
}

/// The other direction: a later registration that needs *less* than the capsule holds is what
/// KHook itself does (`detour.cpp:1695-1719` gives it the existing capsule), so it is accepted.
#[test]
fn capsule_stack_size_accepts_a_smaller_later_registration_on_the_same_address() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_shared(_this: *mut c_void, a: u32) -> u32 {
            a ^ 0xA5
        }

        // 48 bytes first, so the capsule is big enough for `u32 f(this, u32)`.
        let big = unsafe {
            h_void5::setup_inline(
                hooked_shared as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the first registration on the address creates the capsule");
        let small = unsafe {
            h_u32::setup_inline(
                hooked_shared as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("a smaller signature fits the capsule the first one created");

        assert_ne!(big.id(), small.id());
        assert_eq!(hook::live_context_count(), 2);
        assert_eq!(mock.associated(), vec![big.id(), small.id()]);
        drop(big);
        drop(small);
    });
}

/// KHook identifies a virtual hook by the *slot's* address (`vtable + index`,
/// `detour.cpp:1797`), so two entries of one vtable are two capsules: hooking a second index
/// with a bigger signature is fine, and only re-hooking *the same* index with one is refused.
#[test]
fn capsule_stack_size_is_per_vtable_slot() {
    with_mock(|mock| {
        let mut vtable = [
            core::ptr::null_mut::<c_void>(),
            core::ptr::null_mut::<c_void>(),
            core::ptr::null_mut::<c_void>(),
        ];
        let base = vtable.as_mut_ptr();

        let _slot1_small = unsafe {
            h_u32::setup_virtual(base, 1, Vec::new(), Vec::new(), None, SetupMode::Async)
        }
        .expect("the first registration on the slot creates its capsule");
        // A different slot is a different address with its own capsule.
        let _slot2_big = unsafe {
            h_void5::setup_virtual(base, 2, Vec::new(), Vec::new(), None, SetupMode::Async)
        }
        .expect("another slot is another capsule");
        assert_eq!(hook::live_context_count(), 2);

        // The same slot, a signature needing more than its capsule: refused, naming the slot.
        let err = unsafe {
            h_void5::setup_virtual(base, 1, Vec::new(), Vec::new(), None, SetupMode::Async)
        }
        .expect_err("the slot's capsule was created for a smaller stack");
        println!("virtual capsule refusal: {err}");
        match err {
            Error::CapsuleStackTooSmall {
                address,
                capsule,
                needed,
            } => {
                assert_eq!(
                    address,
                    vtable[1..].as_ptr() as usize,
                    "the key is the slot's own address, not the vtable's"
                );
                assert_eq!(capsule, (h_u32::STACK_SIZE + 0xF) & !0xF);
                assert_eq!(needed, (h_void5::STACK_SIZE + 0xF) & !0xF);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|c| matches!(c, Call::SetupVirtualHook { .. }))
                .count(),
            2,
            "the refused registration never reached KHook"
        );
        assert_eq!(hook::live_context_count(), 2);
    });
}

/// A registration KHook *refused* gives its capsule claim back: KHook erases a capsule whose
/// first setup failed (`detour.cpp:1706`), so a later registration on that address starts
/// from its own stack size — even a bigger one.
#[test]
fn a_refused_registration_gives_up_its_capsule_claim() {
    with_mock(|mock| {
        let mut vtable = [core::ptr::null_mut::<c_void>()];
        let base = vtable.as_mut_ptr();

        // A small signature first, but KHook refuses the setup (as it does when the detour
        // cannot be built).
        mock.fail_virtual.store(true, Ordering::SeqCst);
        unsafe { h_u32::setup_virtual(base, 0, Vec::new(), Vec::new(), None, SetupMode::Async) }
            .expect_err("the mock refuses this virtual hook");
        mock.fail_virtual.store(false, Ordering::SeqCst);

        // If the refused registration had kept its capsule, this one — needing more — would be
        // refused too.
        let _hook = unsafe {
            h_void5::setup_virtual(base, 0, Vec::new(), Vec::new(), None, SetupMode::Async)
        }
        .expect("the refused registration gave up its capsule claim");
        assert_eq!(hook::live_context_count(), 1);
    });
}

// -----------------------------------------------------------------------------------------
// 4. Negative paths
// -----------------------------------------------------------------------------------------

/// (a) A refused `SetupVirtualHook` must not produce a live `Hook`.
#[test]
fn negative_virtual_hook_refused_returns_err() {
    with_mock(|mock| {
        mock.fail_virtual.store(true, Ordering::SeqCst);
        let mut vtable = [
            core::ptr::null_mut::<c_void>(),
            core::ptr::null_mut::<c_void>(),
        ];
        // A refused registration must free the context it built: no removal function will
        // ever run for it, so nothing else could.
        let live = Arc::new(());
        let weak = Arc::downgrade(&live);
        let pre: Vec<h_u32::PreCb> = vec![Arc::new({
            let live = Arc::clone(&live);
            move |_call: &mut HookCall, _this: *mut c_void, _a: u32| {
                let _ = &live;
                Return::ignore(0u32)
            }
        })];
        drop(live);
        let err = unsafe {
            h_u32::setup_virtual(
                vtable.as_mut_ptr(),
                1,
                pre,
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect_err("INVALID_HOOK must not become a live Hook");
        println!("refused virtual hook: {err}");
        assert!(
            weak.upgrade().is_none(),
            "the refused registration freed its context"
        );
        assert!(
            matches!(
                err,
                Error::HookSetupFailed {
                    virtual_hook: true
                }
            ),
            "unexpected error: {err}"
        );
        assert_eq!(
            err.to_string(),
            "KHook::SetupVirtualHook returned INVALID_HOOK (0xFFFFFFFF)"
        );
        // Nothing was registered, so nothing may be removed either.
        assert!(mock.removal_calls().is_empty());
        assert!(!mock
            .calls()
            .iter()
            .any(|c| matches!(c, Call::RemoveHook { .. })));
    });
}

/// (b) A declared stack size that disagrees with the computed one is refused before KHook
/// sees it, naming both numbers.
#[test]
fn negative_declared_stack_size_rejected() {
    with_mock(|mock| {
        let computed = h_u32::STACK_SIZE;
        let declared = computed - 8;
        let err = unsafe {
            h_u32::setup_inline(
                core::ptr::null_mut(),
                Vec::new(),
                Vec::new(),
                Some(declared),
                SetupMode::Async,
            )
        }
        .expect_err("a too-small stack size must be refused");
        println!("declared-vs-computed: {err}");
        assert_eq!(
            err.to_string(),
            format!(
                "declared stack size {} does not match the computed stack size {}",
                computed - 8,
                computed
            )
        );
        match err {
            Error::StackSizeMismatch {
                declared: d,
                computed: c,
            } => {
                assert_eq!(d, computed - 8);
                assert_eq!(c, computed);
            }
            other => panic!("unexpected error: {other}"),
        }
        // Refused *before* registering anything, so KHook never saw the call.
        assert!(mock.calls().is_empty(), "the check runs before SetupHook");
    });
}

/// The same check accepts the declared value when it matches (the generated-IR path).
#[test]
fn declared_stack_size_accepted_when_equal() {
    with_mock(|mock| {
        unsafe extern "C" fn hooked_u32(_this: *mut c_void, a: u32) -> u32 {
            a
        }
        let hook = unsafe {
            h_u32::setup_inline(
                hooked_u32 as *mut c_void,
                Vec::new(),
                Vec::new(),
                Some(h_u32::STACK_SIZE),
                SetupMode::Async,
            )
        }
        .expect("a matching declared stack size is accepted");
        assert_ne!(
            hook.id(),
            hook::INVALID_HOOK,
            "KHook answered a live id (the counter is process-wide, `detour.cpp:1548`)"
        );
        assert_eq!(mock.saves(), Vec::new());
    });
}
