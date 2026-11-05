//! A `KHookApi` mock for the library's own unload tests: it registers virtual hooks and
//! **defers** every asynchronous removal to [`step_removals`], the way KHook's delete thread
//! does (`detour.cpp:1852-1867`).
//!
//! That deferral is the point. A synchronous removal (`async = false`, what
//! [`s2sdk::hook::remove_all_blocking`] issues) runs the removal function inside the call; an
//! asynchronous one (`Drop`, `khook.hpp:673`) is queued, and KHook runs the removal function
//! from its own thread some time later. The unload path cares about what that later run finds:
//! with the API still installed it releases the context, without one it marks the registry
//! `unbarriered_removal` (`s2sdk::hook`) and every later `remove_all_blocking` answers
//! `KHookNotInstalled`.
//!
//! Only the library's test binary can use this (`cfg(test)` in `lib.rs`); the integration tests
//! carry their own mocks (`tests/gameframe_support/mod.rs`, `tests/khook_identity.rs`).

#![allow(clippy::missing_panics_doc)]

use core::cell::RefCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use s2sdk::hook::{self, Action, Hook, HookId, KHookApi, Return, SetupMode};
use s2sdk::khook_thunks;

khook_thunks! {
    /// `void target(this)`: the shape these cases hook. The callbacks do nothing - the cases are
    /// about the removal path, not about a call.
    pub(crate) mod target: fn(this: *mut c_void) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

/// The slot these cases register on, in a table of [`SLOTS`] entries.
const INDEX: i32 = 1;
const SLOTS: usize = 2;

/// The ids this mock hands out. Deliberately far from the counter every other mock in this test
/// binary starts at: `s2sdk::hook` remembers every id whose removal function already ran for the
/// life of the process (`Registry::removal_fired`), and an id another case already retired makes
/// [`s2sdk::hook`]'s removal path ignore this mock's hook.
static NEXT_ID: AtomicU32 = AtomicU32::new(0xE000_0001);

static MOCK: Mock = Mock::new();

/// The mock `IKHook`; [`install`] puts it in the process-wide slot.
pub(crate) struct Mock {
    /// `(id, the registered removal function, its context)`, as KHook stores them.
    held: Mutex<Vec<(HookId, usize, usize)>>,
    /// Asynchronous removals KHook has queued and its delete thread has not run yet.
    queued: Mutex<Vec<HookId>>,
    /// `SetupVirtualHook` answers [`hook::INVALID_HOOK`] when set.
    pub(crate) fail: AtomicBool,
}

impl Mock {
    const fn new() -> Self {
        Self {
            held: Mutex::new(Vec::new()),
            queued: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        }
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Run one queued removal the way KHook's delete thread does
    /// (`detour.cpp:1838-1843,1536-1541`): with the registering context pushed, so the wrapper's
    /// removal function reads it back through `GetContextPtr`.
    fn run_removal(&self, id: HookId) {
        let entry = {
            let mut held = Self::lock(&self.held);
            match held.iter().position(|(held_id, _, _)| *held_id == id) {
                Some(index) => held.remove(index),
                None => return,
            }
        };
        let (_, removed, context) = entry;
        // SAFETY: `removed` is the function the wrapper registered for `void f(this)`
        // (`khook.hpp:711-719`), a `fn(HookId)` called with the context pushed.
        unsafe {
            CONTEXT.with(|stack| stack.borrow_mut().push(context));
            let run: unsafe extern "C" fn(HookId) = core::mem::transmute(removed);
            run(id);
            CONTEXT.with(|stack| stack.borrow_mut().pop());
        }
    }
}

thread_local! {
    /// KHook's per-thread context stack (`detour.cpp:359-361`), read by `GetContextPtr`.
    static CONTEXT: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

impl KHookApi for Mock {
    unsafe fn setup_hook(
        &self,
        _function: *mut c_void,
        context: *mut c_void,
        _removed_function: *mut c_void,
        _pre: *mut c_void,
        _post: *mut c_void,
        _make_return: *mut c_void,
        _make_call_original: *mut c_void,
        _stack_size: u32,
        _async: bool,
    ) -> HookId {
        // SAFETY: the wrapper hands its context reference to the API; this mock refuses it.
        unsafe { hook::release(context) };
        unreachable!("these cases register a virtual hook")
    }

    unsafe fn setup_virtual_hook(
        &self,
        _vtable: *mut *mut c_void,
        _index: i32,
        context: *mut c_void,
        removed_function: *mut c_void,
        _pre: *mut c_void,
        _post: *mut c_void,
        _make_return: *mut c_void,
        _make_call_original: *mut c_void,
        _stack_size: u32,
        _async: bool,
    ) -> HookId {
        assert!(!removed_function.is_null(), "KHook needs the removal function");
        assert!(!context.is_null());
        if self.fail.load(Ordering::SeqCst) {
            return hook::INVALID_HOOK;
        }
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        Self::lock(&self.held).push((id, removed_function as usize, context as usize));
        id
    }

    unsafe fn remove_hook(
        &self,
        id: HookId,
        r#async: bool,
        _hook_removal_fn: *mut c_void,
        _context: *mut c_void,
    ) {
        if id == hook::INVALID_HOOK {
            // `remove_all_blocking`'s barrier (`s2sdk::hook`): names no hook, runs nothing.
            return;
        }
        if r#async {
            // KHook's delete thread runs it later (`detour.cpp:1852-1867`).
            Self::lock(&self.queued).push(id);
        } else {
            // A synchronous removal returns only once the removal function has retired
            // (`detour.cpp:1581-1597`).
            self.run_removal(id);
        }
    }

    fn get_context_ptr(&self) -> *mut c_void {
        CONTEXT.with(|stack| {
            stack
                .borrow()
                .last()
                .map_or(core::ptr::null_mut(), |&context| context as *mut c_void)
        })
    }

    fn get_original_function(&self) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn get_original_value_ptr(&self) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn get_override_value_ptr(&self) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn get_current_value_ptr(&self, _pop: bool) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn destroy_return_value(&self) {}

    fn find_original(&self, function: *mut c_void) -> *mut c_void {
        function
    }

    fn find_original_virtual(&self, vtable: *mut *mut c_void, index: i32) -> *mut c_void {
        // SAFETY: the caller guarantees the slot is readable (`s2sdk::hook`'s contract).
        unsafe { *vtable.offset(index as isize) }
    }

    unsafe fn do_recall(
        &self,
        _action: Action,
        _ptr_to_return: *mut c_void,
        _return_size: usize,
        _init_op: *mut c_void,
        _deinit_op: *mut c_void,
    ) -> *mut c_void {
        unreachable!("these cases never recall a hook")
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
    }

    fn lookup_signature(
        &self,
        _start: *mut c_void,
        _size: usize,
        _signature: *const core::ffi::c_char,
    ) -> *mut c_void {
        unreachable!("these cases never look a signature up")
    }

    fn was_original_function_skipped(&self) -> bool {
        false
    }
}

/// Forgets every registration, queued removal and record: call before each case.
pub(crate) fn reset() {
    Mock::lock(&MOCK.held).clear();
    Mock::lock(&MOCK.queued).clear();
    MOCK.fail.store(false, Ordering::SeqCst);
}

/// Installs the mock in the process-wide API slot ([`s2sdk::hook::install`]).
pub(crate) fn install() {
    hook::install(&MOCK);
}

/// How many asynchronous removals KHook has queued and not run yet.
pub(crate) fn queued() -> usize {
    Mock::lock(&MOCK.queued).len()
}

/// Runs every queued removal, as KHook's delete thread would, and answers how many ran.
pub(crate) fn step_removals() -> usize {
    let ids: Vec<HookId> = Mock::lock(&MOCK.queued).drain(..).collect();
    for id in &ids {
        MOCK.run_removal(*id);
    }
    ids.len()
}

/// Registers one `void target(this)` hook asynchronously - the `SetupMode::Async` the GameFrame
/// install uses - and answers it: its removal stays queued until [`step_removals`], exactly like
/// a `Drop` whose delete thread has not run.
pub(crate) fn register_async() -> Hook {
    /// The hooked address (never called: the mock builds no trampoline).
    unsafe extern "C" fn original(_this: *mut c_void) {}
    let table = Box::leak(vec![original as *mut c_void; SLOTS].into_boxed_slice());
    let pre: Vec<target::PreCb> = vec![std::sync::Arc::new(|_call, _this| Return::ignore(()))];
    let post: Vec<target::PostCb> = vec![std::sync::Arc::new(|_call, _this| Return::ignore(()))];
    // SAFETY: `table` is a leaked, writable table of `SLOTS` entries and `INDEX` is inside it;
    // the thunks were generated for exactly `void f(this)`.
    unsafe { target::setup_virtual(table.as_mut_ptr(), INDEX, pre, post, None, SetupMode::Async) }
        .expect("the mock accepts the registration")
}
