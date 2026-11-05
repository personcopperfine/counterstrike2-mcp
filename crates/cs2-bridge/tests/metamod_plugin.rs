//! The Metamod half of the plugin, driven through the generated vtables with a mock `ISmmAPI`
//! and a mock `IKHook`.
//!
//! Nothing here calls the plugin's functions directly: every call goes through the table entry
//! Metamod would call (the generated `_vtable` field or a generated `_slots` index, never a
//! hand-written number), so the vtable layout and the argument lists are part of what is
//! tested.
//!
//! The two mocks are real objects of the generated types (`ISmmAPI`, `IKHook`) whose tables
//! hold this file's functions. Every mock function cites the header line it mirrors and checks
//! the arguments its caller's contract promises; it records a violation instead of asserting
//! (an `assert!` would unwind across the C ABI), and the test body fails on the recorded list -
//! a mock that accepted anything would hide exactly the bugs these tests exist for.
//!
//! The API slot of `s2sdk::hook` is process-wide, so the cases serialize on [`TEST_LOCK`].

use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::ThreadId;

use cs2_bridge::ops::metamod;
use cs2_bridge::{dispatch, registry, wiring, iface, plugin, toolsys};
use cs2_bridge::ipc::Route;
use cs2_bridge::modules::ModuleViews;
use s2sdk::hook::{self, HookId, SetupMode};
use s2sdk::khook_thunks;
use s2sdk_sys::generated::{
    IMetamodListener, ISmmAPI, ISmmAPI_vtable, ISmmPlugin, ISmmPlugin_slots, ISmmPluginManager,
    ISmmPluginManager_vtable, IKHook, IKHook_vtable, METAMOD_PLAPI_VERSION, MMIFACE_PLMANAGER,
    Pl_Status,
};

// ---------------------------------------------------------------------------------------
// Recording mock-contract violations
// ---------------------------------------------------------------------------------------

/// Every argument its caller's contract promises but did not deliver, as seen by the mocks.
///
/// The mocks are `extern "C"` entries called by the code under test: an `assert!`/`panic!` in
/// one of them would try to unwind across the C ABI (Rust aborts the process), and a panic in
/// [`Reset::drop`] during an unwinding test would abort too. So they **record** the violation
/// and answer a neutral value, and the test body (via [`with_plugin`]) asserts the list is
/// empty. A mock that accepted anything would still hide bugs, so every check below is real -
/// it just reports at the test level.
static VIOLATIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record one contract violation; callable from `extern "C"` mocks and from `Drop`.
fn record_violation(message: String) {
    VIOLATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(message);
}

/// `record_violation(format!(...))` when `condition` is false.
fn check(condition: bool, message: impl FnOnce() -> String) {
    if !condition {
        record_violation(message());
    }
}

/// Take everything recorded since the last call (the test body has just run).
fn take_violations() -> Vec<String> {
    std::mem::take(
        &mut *VIOLATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

// ---------------------------------------------------------------------------------------
// A hookable signature for the drain test
// ---------------------------------------------------------------------------------------

khook_thunks! {
    /// `void f(this)`: the shape of every hook this file registers. The mock never installs
    /// the detour, so the body only has to exist.
    pub mod h_void: fn(this: *mut c_void) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

/// The target of the registration; never called (the mock does not detour anything).
unsafe extern "C" fn hooked_void(_this: *mut c_void) {}

// ---------------------------------------------------------------------------------------
// The mock IKHook
// ---------------------------------------------------------------------------------------

/// One `RemoveHook` call the mock saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Removal {
    id: HookId,
    /// `IKHook::RemoveHook`'s `async` argument (`khook.hpp:2188`).
    r#async: bool,
    /// `hook_removal_fn` (`khook.hpp:2188`): what the wrapper passes.
    hook_removal_fn: usize,
    /// `context` (`khook.hpp:2188`).
    context: usize,
}

/// One `SetupHook` call the mock saw, with the arguments that matter to the wrapper's contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Setup {
    id: HookId,
    /// `IKHook::SetupHook`'s `async` argument (`khook.hpp:2186`): `SetupMode::is_async`'s value,
    /// forwarded verbatim.
    r#async: bool,
    /// `stack_size` (`khook.hpp:2186`): the copy stack the wrapper computed for the signature.
    stack_size: u32,
}

/// What the mock's `IKHook` remembers: the hooks it is holding, the calls it has seen, and the
/// two switches the negative cases need.
struct MockKhook {
    next_id: AtomicUsize,
    /// `HookID_t` -> the removal function and context KHook stored for it
    /// (`detour.cpp:1536-1541` passes the *registering* context to the removal function).
    held: Mutex<Vec<(HookId, usize, usize)>>,
    removals: Mutex<Vec<Removal>>,
    /// Every `SetupHook` call, in order.
    setups: Mutex<Vec<Setup>>,
    /// Model KHook's insert-thread window (`detour.cpp:1607-1613`): the **next** registration's
    /// entry is held by the insert thread — associated (`detour.cpp:1733-1736`), not in the
    /// capsule's `_callbacks`, not in `insert_hooks` — instead of being inserted. It has to be
    /// armed *before* that registration: a hook that reached [`MockKhook::held`] is in the
    /// capsule, and a synchronous removal of it always runs its removal function
    /// (`detour.cpp:1508-1543`).
    pop_next: AtomicBool,
    /// The entries `pop_next` took. A synchronous removal of one of them finds the id associated
    /// but no callback, so `DetourCapsule::RemoveHook` answers "true" **without** running the
    /// removal function (`detour.cpp:1508-1543`, `:1582-1589`); the insert thread then finds the
    /// id disassociated and drops the details (`:1566-1571`). That is the lost removal, and the
    /// entry moves to [`MockKhook::leaked`].
    window: Mutex<Vec<(HookId, usize, usize)>>,
    /// The entries a lost removal erased, kept so a case's cleanup can release the reference the
    /// leak left behind. KHook itself never touches these.
    leaked: Mutex<Vec<(HookId, usize, usize)>>,
}

impl MockKhook {
    const fn new() -> Self {
        Self {
            next_id: AtomicUsize::new(1),
            held: Mutex::new(Vec::new()),
            removals: Mutex::new(Vec::new()),
            setups: Mutex::new(Vec::new()),
            pop_next: AtomicBool::new(false),
            window: Mutex::new(Vec::new()),
            leaked: Mutex::new(Vec::new()),
        }
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn removals(&self) -> Vec<Removal> {
        Self::lock(&self.removals).clone()
    }

    fn setups(&self) -> Vec<Setup> {
        Self::lock(&self.setups).clone()
    }

    fn held_len(&self) -> usize {
        Self::lock(&self.held).len()
    }

    /// How many entries the modelled insert thread holds (associated, no capsule callback).
    fn window_len(&self) -> usize {
        Self::lock(&self.window).len()
    }

    /// The ids the modelled insert thread holds, oldest first.
    fn window_ids(&self) -> Vec<HookId> {
        Self::lock(&self.window).iter().map(|(id, _, _)| *id).collect()
    }

    /// How many entries a lost removal erased without running their removal function.
    fn leaked_len(&self) -> usize {
        Self::lock(&self.leaked).len()
    }

    /// Harness bookkeeping for a case that modelled KHook losing a hook: run the removal
    /// functions of the entries the lost removal left behind. This is no longer the only thing
    /// that lowers the process-wide live-context count - the wrapper's reconcile lowered it
    /// when it detected the lost removal - but it is the only way this mock can hand those
    /// entries back to the wrapper's removal function. **KHook never does this** - the leak the
    /// case models is permanent - it exists so the rest of this test binary starts from zero
    /// and so the late call is seen to be ignored.
    fn release_leaked(&self) {
        let entries: Vec<(HookId, usize, usize)> = Self::lock(&self.leaked).drain(..).collect();
        for (id, removed_fn, context) in entries {
            // SAFETY: `removed_fn` is the address the wrapper registered for this context and
            // `context` is the context it registered with - what KHook would have pushed had it
            // not lost the hook.
            unsafe {
                THREAD_CONTEXT.with(|stack| stack.borrow_mut().push(context));
                let removed: unsafe extern "C" fn(HookId) = core::mem::transmute(removed_fn);
                removed(id);
                THREAD_CONTEXT.with(|stack| stack.borrow_mut().pop());
            }
        }
    }

    /// Forget every call recorded so far, so each case starts from an empty history.
    fn reset(&self) {
        Self::lock(&self.held).clear();
        Self::lock(&self.window).clear();
        Self::lock(&self.leaked).clear();
        Self::lock(&self.removals).clear();
        Self::lock(&self.setups).clear();
        self.pop_next.store(false, Ordering::SeqCst);
    }

    /// Harness bookkeeping for a case that left a hook the module can no longer re-issue: run
    /// the removal function of every hook the mock still holds, the way KHook does for a
    /// synchronous removal. Used after `remove_all_blocking` gave the ids up while its API was
    /// missing (it empties the registry *before* it checks the API), which is a state only a
    /// probe can create.
    fn release_held(&self) {
        let ids: Vec<HookId> = Self::lock(&self.held)
            .iter()
            .map(|(id, _, _)| *id)
            .collect();
        for id in ids {
            self.run_removal(id);
        }
    }

    /// Run one held hook's removal function the way KHook does for a synchronous removal
    /// (`detour.cpp:1581-1597` -> `:1536-1541`): with the registering context pushed, so
    /// `GetContextPtr` inside it answers that context.
    fn run_removal(&self, id: HookId) {
        let entry = {
            let mut held = Self::lock(&self.held);
            match held.iter().position(|(held_id, _, _)| *held_id == id) {
                Some(index) => held.remove(index),
                // "not associated" (`detour.cpp:1581-1587`): nothing to run.
                None => return,
            }
        };
        let (_, removed_fn, context) = entry;
        // SAFETY: `removed_fn` is the address the wrapper registered for this id and `context`
        // is the context it registered with - exactly what KHook hands the removal function.
        unsafe {
            THREAD_CONTEXT.with(|stack| stack.borrow_mut().push(context));
            let removed: unsafe extern "C" fn(HookId) = core::mem::transmute(removed_fn);
            removed(id);
            THREAD_CONTEXT.with(|stack| stack.borrow_mut().pop());
        }
    }
}

thread_local! {
    /// KHook's per-thread context stack (`g_current_hook`, `detour.cpp:359-361`), which is
    /// what `IKHook::GetContextPtr` reads.
    static THREAD_CONTEXT: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The mock `IKHook` object (one vtable pointer, `khook.hpp:2184`).
static MOCK_KHOOK: MockKhook = MockKhook::new();

/// The mock's `IKHook`: every virtual of `khook.hpp:2186-2200`, at the generated slot.
static MOCK_KHOOK_VTABLE: IKHook_vtable = IKHook_vtable {
    SetupHook: Some(mock_setup_hook),
    SetupVirtualHook: Some(mock_setup_virtual_hook),
    RemoveHook: Some(mock_remove_hook),
    GetContextPtr: Some(mock_get_context_ptr),
    GetOriginalFunction: Some(mock_get_original_function),
    GetOriginalValuePtr: Some(mock_get_original_value_ptr),
    GetOverrideValuePtr: Some(mock_get_override_value_ptr),
    GetCurrentValuePtr: Some(mock_get_current_value_ptr),
    DestroyReturnValue: Some(mock_destroy_return_value),
    FindOriginal: Some(mock_find_original),
    FindOriginalVirtual: Some(mock_find_original_virtual),
    DoRecall: Some(mock_do_recall),
    SaveReturnValue: Some(mock_save_return_value),
    LookupSignature: Some(mock_lookup_signature),
    WasOriginalFunctionSkipped: Some(mock_was_original_function_skipped),
};

/// The mock's `IKHook` object. The generated struct holds a raw vtable pointer, so sharing it
/// needs the same justification as any other: the table above is a `static` that nothing
/// writes.
struct KhookObj(IKHook);

// SAFETY: the object's only field is a pointer to the immutable `MOCK_KHOOK_VTABLE` static.
unsafe impl Sync for KhookObj {}

static MOCK_KHOOK_OBJ: KhookObj = KhookObj(IKHook {
    vtable: ptr::addr_of!(MOCK_KHOOK_VTABLE).cast::<c_void>(),
});

/// The only `IKHook` object this plugin may call: the mock's own.
fn expected_khook() -> usize {
    ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut() as usize
}

/// `IKHook::SetupHook` (`khook.hpp:2186`): `HookID_t SetupHook(void* function, void* context,
/// void* removed_function, void* pre, void* post, void* make_return, void* make_call_original,
/// unsigned int stack_size, bool async)`.
unsafe extern "C" fn mock_setup_hook(
    this: *mut IKHook,
    function: *mut c_void,
    context: *mut c_void,
    removed_function: *mut c_void,
    pre: *mut c_void,
    post: *mut c_void,
    make_return: *mut c_void,
    make_call_original: *mut c_void,
    stack_size: u32,
    r#async: bool,
) -> u32 {
    check(this as usize == expected_khook(), || {
        format!("IKHook::SetupHook on {this:p}, not the IKHook GetDetourInterface returned")
    });
    // The wrapper's own contract: a live function, a live context, and all four thunks
    // (`s2sdk::hook::register_inline`'s safety contract).
    check(!function.is_null(), || "SetupHook without a target".to_owned());
    check(!context.is_null(), || "SetupHook without a context".to_owned());
    check(!removed_function.is_null(), || {
        "SetupHook without a removal function".to_owned()
    });
    check(!pre.is_null() && !post.is_null(), || {
        "SetupHook without callbacks".to_owned()
    });
    check(!make_return.is_null() && !make_call_original.is_null(), || {
        "SetupHook without the value thunks".to_owned()
    });
    check(stack_size > 0, || "SetupHook with a zero copy stack".to_owned());
    let id = MOCK_KHOOK.next_id.fetch_add(1, Ordering::SeqCst) as HookId;
    // `K:1607-1613`: with the window armed, the insert thread has popped this entry and holds
    // it. The id is associated (KHook did that before the queue, `detour.cpp:1733-1736`) but the
    // capsule has no callback for it.
    let slot = if MOCK_KHOOK.pop_next.swap(false, Ordering::SeqCst) {
        &MOCK_KHOOK.window
    } else {
        &MOCK_KHOOK.held
    };
    MockKhook::lock(slot).push((id, removed_function as usize, context as usize));
    MockKhook::lock(&MOCK_KHOOK.setups).push(Setup {
        id,
        r#async,
        stack_size,
    });
    id
}

/// `IKHook::SetupVirtualHook` (`khook.hpp:2187`): the same, with `(void** vtable, int index)`
/// in place of the function pointer. This plugin registers no virtual hook, so reaching it is a
/// failure of the code under test, not a case to model.
unsafe extern "C" fn mock_setup_virtual_hook(
    _this: *mut IKHook,
    _vtable: *mut *mut c_void,
    _index: i32,
    _context: *mut c_void,
    _removed_function: *mut c_void,
    _pre: *mut c_void,
    _post: *mut c_void,
    _make_return: *mut c_void,
    _make_call_original: *mut c_void,
    _stack_size: u32,
    _is_async: bool,
) -> u32 {
    record_violation("IKHook::SetupVirtualHook: this plugin registers no virtual hook".to_owned());
    hook::INVALID_HOOK
}

/// `IKHook::RemoveHook` (`khook.hpp:2188`): `void RemoveHook(HookID_t id, bool async = false,
/// void (*hook_removal_fn)(HookID_t, void*) = nullptr, void* context = nullptr)`.
///
/// A synchronous removal runs the hook's removal function before it returns
/// (`detour.cpp:1581-1597`); an asynchronous one only queues it (`detour.cpp:1852-1867`), which
/// this mock models by doing nothing - the drain must re-issue it synchronously, and the test
/// checks that it did.
unsafe extern "C" fn mock_remove_hook(
    this: *mut IKHook,
    id: HookId,
    r#async: bool,
    hook_removal_fn: *const c_void,
    context: *mut c_void,
) {
    check(this as usize == expected_khook(), || {
        format!("IKHook::RemoveHook on {this:p}, not the IKHook GetDetourInterface returned")
    });
    // The wrapper passes null for both caller-supplied slots (`khook.hpp:2188`'s defaults; see
    // `Hook::drop`).
    check(hook_removal_fn.is_null() && context.is_null(), || {
        format!("RemoveHook with a caller-supplied removal callback: id {id}")
    });
    MockKhook::lock(&MOCK_KHOOK.removals).push(Removal {
        id,
        r#async,
        hook_removal_fn: hook_removal_fn as usize,
        context: context as usize,
    });
    if r#async {
        // Queued for the delete thread (`detour.cpp:1852-1867`); it never gets there in these
        // tests, which is what makes the drain's re-issue observable.
        return;
    }
    // The insert-thread window: the id is associated but the capsule has no callback for it, so
    // `DetourCapsule::RemoveHook` answers "gone" and calls nothing (`detour.cpp:1508-1543`);
    // `__RemoveHook_Sync` then erases the association (`:1587-1589`) and the insert thread drops
    // the details when it finds the id unassociated (`:1566-1571`). **No removal function runs,
    // ever** - the one state in which a *synchronous* `RemoveHook` returns without it (the loop
    // at `:1579-1597` otherwise waits for the capsule). The entry moves to `leaked`, which only a
    // case's own cleanup reads: running those removal functions is harness bookkeeping, never
    // something KHook does.
    let entry = {
        let mut window = MockKhook::lock(&MOCK_KHOOK.window);
        window
            .iter()
            .position(|(window_id, _, _)| *window_id == id)
            .map(|index| window.remove(index))
    };
    if let Some(entry) = entry {
        MockKhook::lock(&MOCK_KHOOK.leaked).push(entry);
        return;
    }
    MOCK_KHOOK.run_removal(id);
}

/// `IKHook::GetContextPtr` (`khook.hpp:2189`): `void* GetContextPtr()` - the context pushed
/// for the running call (`detour.cpp:359-361`).
unsafe extern "C" fn mock_get_context_ptr(_this: *mut IKHook) -> *mut c_void {
    THREAD_CONTEXT.with(|stack| {
        stack
            .borrow()
            .last()
            .map_or(ptr::null_mut(), |&c| c as *mut c_void)
    })
}

/// `IKHook::GetOriginalFunction` (`khook.hpp:2190`): not reached by this plugin.
unsafe extern "C" fn mock_get_original_function(_this: *mut IKHook) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::GetOriginalValuePtr` (`khook.hpp:2191`): not reached by this plugin.
unsafe extern "C" fn mock_get_original_value_ptr(_this: *mut IKHook) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::GetOverrideValuePtr` (`khook.hpp:2192`): not reached by this plugin.
unsafe extern "C" fn mock_get_override_value_ptr(_this: *mut IKHook) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::GetCurrentValuePtr` (`khook.hpp:2193`): not reached by this plugin.
unsafe extern "C" fn mock_get_current_value_ptr(_this: *mut IKHook, _pop: bool) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::DestroyReturnValue` (`khook.hpp:2194`): not reached by this plugin.
unsafe extern "C" fn mock_destroy_return_value(_this: *mut IKHook) {}

/// `IKHook::FindOriginal` (`khook.hpp:2195`): not reached by this plugin.
unsafe extern "C" fn mock_find_original(_this: *mut IKHook, _function: *mut c_void) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::FindOriginalVirtual` (`khook.hpp:2196`): not reached by this plugin.
unsafe extern "C" fn mock_find_original_virtual(
    _this: *mut IKHook,
    _vtable: *mut *mut c_void,
    _index: i32,
) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::DoRecall` (`khook.hpp:2197`): not reached by this plugin.
unsafe extern "C" fn mock_do_recall(
    _this: *mut IKHook,
    _action: u8,
    _ptr_to_return: *mut c_void,
    _return_size: usize,
    _init_op: *mut c_void,
    _deinit_op: *mut c_void,
) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::SaveReturnValue` (`khook.hpp:2198`): not reached by this plugin.
unsafe extern "C" fn mock_save_return_value(
    _this: *mut IKHook,
    _action: u8,
    _ptr_to_return: *mut c_void,
    _return_size: usize,
    _init_op: *mut c_void,
    _deinit_op: *mut c_void,
    _original: bool,
) {
}

/// `IKHook::LookupSignature` (`khook.hpp:2199`): not reached by this plugin.
unsafe extern "C" fn mock_lookup_signature(
    _this: *mut IKHook,
    _start: *mut c_void,
    _size: usize,
    _signature: *const c_char,
) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::WasOriginalFunctionSkipped` (`khook.hpp:2200`): not reached by this plugin.
unsafe extern "C" fn mock_was_original_function_skipped(_this: *mut IKHook) -> bool {
    false
}

// ---------------------------------------------------------------------------------------
// The mock ISmmAPI
// ---------------------------------------------------------------------------------------

/// The plugin interface name, as a C string: the generated `METAMOD_PLAPI_NAME`, with the
/// terminator Metamod's `strcmp` needs (`core/ISmmPlugin.h:478`).
const PLAPI_NAME_C: &core::ffi::CStr = c"ISmmPlugin";

/// The plugin id the mock Metamod hands `Load`. Non-zero and not the C++ sample's, so a
/// hard-coded `0` would show. It is also the id every case's `Load` uses unless the case says
/// otherwise ([`EXPECTED_DETOUR_ID`]).
const MOCK_PLUGIN_ID: c_int = 7;

/// The id the mock's `GetDetourInterface` must be handed: `PLUGIN_SAVEVARS` passes the id its
/// `Load` was called with (`core/ISmmPlugin.h:516`), and a case that loads the object twice - as
/// `meta retry` does - declares each id before its call. [`with_plugin`] sets the default
/// ([`MOCK_PLUGIN_ID`]).
static EXPECTED_DETOUR_ID: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(MOCK_PLUGIN_ID);

/// What the mock's `ISmmAPI` saw.
struct MockSmm {
    /// The `id` argument of every `GetDetourInterface` call (`ISmmAPI.h:449`).
    detour_ids: Mutex<Vec<c_int>>,
    /// The `(plugin, listener)` pairs `AddListener` was given (`ISmmAPI.h:201`).
    listeners: Mutex<Vec<(usize, usize)>>,
}

impl MockSmm {
    const fn new() -> Self {
        Self {
            detour_ids: Mutex::new(Vec::new()),
            listeners: Mutex::new(Vec::new()),
        }
    }

    fn detour_ids(&self) -> Vec<c_int> {
        self.detour_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn listeners(&self) -> Vec<(usize, usize)> {
        self.listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

static MOCK_SMM: MockSmm = MockSmm::new();

/// `ISmmAPI_vtable` holds `*const c_void` fields for the C variadic virtuals, which makes the
/// generated struct `!Sync`; a table of function addresses is shared by construction.
struct SmmVtable(ISmmAPI_vtable);

// SAFETY: the table holds only function addresses and nulls; nothing writes it.
unsafe impl Sync for SmmVtable {}

/// The mock `ISmmAPI`: the slots this plugin uses are filled, every other one stays
/// null like `ISmmAPI_vtable::NULL` (the plugin must not call them; a null slot would fault
/// and the test would fail). `MetaFactory` is the plugin-table fetch's slot
/// (`ops::metamod::fetch_plugin_manager`).
static MOCK_SMM_VTABLE: SmmVtable = SmmVtable(ISmmAPI_vtable {
    AddListener: Some(mock_add_listener),
    GetDetourInterface: Some(mock_get_detour_interface),
    MetaFactory: Some(mock_meta_factory),
    ..ISmmAPI_vtable::NULL
});

/// The mock `ISmmAPI` object (one vtable pointer, `ISmmAPI.h`'s class).
struct SmmObj(ISmmAPI);

// SAFETY: the object's only field is a pointer to the immutable `MOCK_SMM_VTABLE` static.
unsafe impl Sync for SmmObj {}

static MOCK_SMM_OBJ: SmmObj = SmmObj(ISmmAPI {
    vtable: ptr::addr_of!(MOCK_SMM_VTABLE.0).cast::<c_void>(),
});

/// The only `ISmmAPI` object `Load` may be handed in these cases.
fn expected_smm() -> usize {
    ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut() as usize
}

/// `ISmmAPI::AddListener` (`core/ISmmAPI.h:201`): `void AddListener(ISmmPlugin *plugin,
/// IMetamodListener *pListener)`.
unsafe extern "C" fn mock_add_listener(
    this: *mut ISmmAPI,
    plugin_obj: *mut ISmmPlugin,
    listener: *mut IMetamodListener,
) {
    check(this as usize == expected_smm(), || {
        format!("ISmmAPI::AddListener on {this:p}, not the object Load was given")
    });
    // The C++ sample registers the plugin object and the listener as the same `this`
    // (`samples/s2_sample_mm/src/plugin.cpp:96`); here they are two objects of two classes, and
    // both are this crate's statics - which is what the addresses below check.
    check(plugin_obj == plugin::object().cast::<ISmmPlugin>(), || {
        "AddListener's plugin argument must be the ISmmPlugin object CreateInterface returned"
            .to_owned()
    });
    check(core::ptr::eq(listener, plugin::listener()), || {
        "AddListener's listener argument must be the object OnMetamodQuery is served from"
            .to_owned()
    });
    MockSmm::lock_of(&MOCK_SMM.listeners).push((plugin_obj as usize, listener as usize));
}

impl MockSmm {
    fn lock_of<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `ISmmAPI::GetDetourInterface` (`core/ISmmAPI.h:449`): `void* GetDetourInterface(PluginId
/// id)` - Metamod's `KHook::IKHook*` for that plugin (`core/metamod.cpp:1081`).
unsafe extern "C" fn mock_get_detour_interface(this: *mut ISmmAPI, id: c_int) -> *mut c_void {
    check(this as usize == expected_smm(), || {
        format!("ISmmAPI::GetDetourInterface on {this:p}, not the object Load was given")
    });
    // `PLUGIN_SAVEVARS` passes the `id` `Load` was called with (`core/ISmmPlugin.h:516`), and
    // the case declares which one that is: `meta retry` (`core/metamod_plugins.cpp:351-398`)
    // loads the same file a second time under a *new* id while the old `CPlugin` is still in
    // Metamod's list, so one case runs two `Load`s with two ids.
    let expected = EXPECTED_DETOUR_ID.load(Ordering::SeqCst);
    check(expected != 0, || {
        "the case left no expected plugin id set".to_owned()
    });
    check(id == expected, || {
        format!("GetDetourInterface with plugin id {id}, not Load's {expected}")
    });
    MockSmm::lock_of(&MOCK_SMM.detour_ids).push(id);
    ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut().cast::<c_void>()
}

// ---------------------------------------------------------------------------------------
// The mock ISmmPluginManager (the plugin table's source)
// ---------------------------------------------------------------------------------------

/// One row the mock manager answers `Query` from: a model of one Metamod `CPlugin` - the id
/// Metamod assigned (`metamod_plugins.cpp:461-464`), the file path its `CPlugin` holds, the
/// `Pl_Status` and the loading source (`core/IPluginManager.h:44-61`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MockPlugin {
    id: c_int,
    file: &'static str,
    status: Pl_Status,
    source: c_int,
}

/// One `Query` call the mock saw: the id and the thread that asked - the caller-thread
/// record the NEGATIVE case reads (the bridge's IPC thread must never reach the manager).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueryCall {
    id: c_int,
    thread: ThreadId,
}

/// The mock manager's own file-answer buffer (`MOCK_FILE_BUF_LEN` bytes): `Query`'s `file`
/// answer points INTO it, and the next `Query` for another id rewrites it - exactly the
/// aliasing a real Metamod has (the `CPlugin` owns the string, the header only promises the
/// pointer for the call), which is what the rebuild's immediate copy is measured against.
const MOCK_FILE_BUF_LEN: usize = 256;

/// What the mock `ISmmPluginManager` saw and answers.
struct MockPlmgr {
    plugins: Mutex<Vec<MockPlugin>>,
    calls: Mutex<Vec<QueryCall>>,
    file_buf: Mutex<[c_char; MOCK_FILE_BUF_LEN]>,
    /// Armed by a case: `MetaFactory` refuses the [`MMIFACE_PLMANAGER`] name with
    /// `META_IFACE_FAILED` and a null object - the "IPluginManager unavailable" state a
    /// `Load` must tolerate (`ops::metamod::fetch_plugin_manager`'s contract).
    refuse_metafactory: AtomicBool,
}

impl MockPlmgr {
    const fn new() -> Self {
        Self {
            plugins: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            file_buf: Mutex::new([0 as c_char; MOCK_FILE_BUF_LEN]),
            refuse_metafactory: AtomicBool::new(false),
        }
    }

    fn lock_of<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forget every call and row, so each case starts from an empty manager.
    fn reset(&self) {
        Self::lock_of(&self.plugins).clear();
        Self::lock_of(&self.calls).clear();
        *Self::lock_of(&self.file_buf) = [0 as c_char; MOCK_FILE_BUF_LEN];
        self.refuse_metafactory.store(false, Ordering::SeqCst);
    }

    /// The rows `Query` answers from; an id with no row answers false - the "not found"
    /// (`core/IPluginManager.h`'s doc) a rebuild skips.
    fn set_plugins(&self, rows: &[MockPlugin]) {
        *Self::lock_of(&self.plugins) = rows.to_vec();
    }

    fn calls(&self) -> Vec<QueryCall> {
        Self::lock_of(&self.calls).clone()
    }

    fn clear_calls(&self) {
        Self::lock_of(&self.calls).clear();
    }

    /// Overwrite the file-answer buffer's bytes, after a rebuild: the pointer Metamod gave
    /// out still points here, so a row that kept the pointer would now read this.
    fn corrupt_file_buf(&self, fill: c_char) {
        let mut buf = Self::lock_of(&self.file_buf);
        for byte in buf.iter_mut() {
            *byte = fill;
        }
    }
}

/// The mock manager object (one vtable pointer, like the real `ISmmPluginManager`).
struct PlmgrObj(ISmmPluginManager);

// SAFETY: the object's only field is a pointer to the immutable `MOCK_PLMGR_VTABLE` static.
unsafe impl Sync for PlmgrObj {}

/// The mock manager's vtable: only `Query` is filled (the one slot the snapshot reads);
/// every other entry stays null and a call to one would fault the test - the plugin must
/// never Load/Unload/Pause a plugin (the manager is main-thread state it may only READ).
static MOCK_PLMGR_VTABLE: ISmmPluginManager_vtable = ISmmPluginManager_vtable {
    Query: Some(mock_plmgr_query),
    ..ISmmPluginManager_vtable::NULL
};

static MOCK_PLMGR_OBJ: PlmgrObj = PlmgrObj(ISmmPluginManager {
    vtable: ptr::addr_of!(MOCK_PLMGR_VTABLE).cast::<c_void>(),
});

/// The only `ISmmPluginManager` object the plugin table may touch.
fn expected_plmgr() -> usize {
    ptr::addr_of!(MOCK_PLMGR_OBJ.0).cast_mut() as usize
}

static MOCK_PLMGR: MockPlmgr = MockPlmgr::new();

/// `ISmmPluginManager::Query` (`core/IPluginManager.h:~125`): `bool Query(PluginId id,
/// const char **file, Pl_Status *status, PluginId *source)` - true when the id names a plugin,
/// false when not found, and the `file` answer points into the mock's own buffer (see
/// [`MOCK_FILE_BUF_LEN`]).
unsafe extern "C" fn mock_plmgr_query(
    this: *mut ISmmPluginManager,
    id: c_int,
    file: *mut *const c_char,
    status: *mut Pl_Status,
    source: *mut c_int,
) -> bool {
    check(this as usize == expected_plmgr(), || {
        format!("ISmmPluginManager::Query on {this:p}, not the object MetaFactory returned")
    });
    check(!file.is_null(), || "Query without a file out-pointer".to_owned());
    MockPlmgr::lock_of(&MOCK_PLMGR.calls).push(QueryCall {
        id,
        thread: std::thread::current().id(),
    });
    let row = MockPlmgr::lock_of(&MOCK_PLMGR.plugins)
        .iter()
        .copied()
        .find(|row| row.id == id);
    let Some(row) = row else {
        return false;
    };
    let mut buf = MockPlmgr::lock_of(&MOCK_PLMGR.file_buf);
    let bytes = row.file.as_bytes();
    check(
        bytes.len() < MOCK_FILE_BUF_LEN,
        || "the mock's file name does not fit its own buffer".to_owned(),
    );
    for (i, &byte) in bytes.iter().enumerate() {
        buf[i] = byte as c_char;
    }
    buf[bytes.len()] = 0;
    // SAFETY: the caller guarantees `file` is null or a writable out-pointer (the call's
    // contract); the answer points into the mock's buffer, which outlives the call.
    unsafe { *file = buf.as_ptr() };
    if !status.is_null() {
        // SAFETY: as above, for `status`.
        unsafe { *status = row.status };
    }
    if !source.is_null() {
        // SAFETY: as above, for `source`.
        unsafe { *source = row.source };
    }
    true
}

/// `ISmmAPI::MetaFactory` (`core/ISmmAPI.h:212`, the shape of the generated slot): Metamod's
/// answer for [`MMIFACE_PLMANAGER`] is its own manager object with `*ret = META_IFACE_OK`
/// (`core/metamod.cpp:813-821`, a `strcmp` on the name, `&g_PluginMngr`). The id out-pointer
/// may be null - the plugin passes null (Metamod's own implementation tests every pointer,
/// `if (ret)`); the mock does not read it.
unsafe extern "C" fn mock_meta_factory(
    this: *mut ISmmAPI,
    iface: *const c_char,
    ret: *mut c_int,
    id: *mut c_int,
) -> *mut c_void {
    check(this as usize == expected_smm(), || {
        format!("ISmmAPI::MetaFactory on {this:p}, not the object Load was given")
    });
    let _ = id;
    if MOCK_PLMGR.refuse_metafactory.load(Ordering::SeqCst) {
        if !ret.is_null() {
            // SAFETY: the caller guarantees `ret` is null or writable.
            unsafe { *ret = plugin::META_IFACE_FAILED };
        }
        return ptr::null_mut();
    }
    let name_ok = !iface.is_null()
        // SAFETY: the caller's contract - a NUL-terminated name for the duration of the call.
        && unsafe { core::ffi::CStr::from_ptr(iface) }.to_bytes() == MMIFACE_PLMANAGER.as_bytes();
    if !name_ok {
        record_violation(format!(
            "MetaFactory asked for a name this mock does not serve (wanted {MMIFACE_PLMANAGER})"
        ));
        return ptr::null_mut();
    }
    if !ret.is_null() {
        // SAFETY: as above.
        unsafe { *ret = plugin::META_IFACE_OK };
    }
    ptr::addr_of!(MOCK_PLMGR_OBJ.0).cast_mut().cast::<c_void>()
}

// ---------------------------------------------------------------------------------------
// Calling the plugin through its own tables
// ---------------------------------------------------------------------------------------

/// Serializes the cases: `s2sdk::hook`'s API slot is process-wide and the plugin's own state
/// (`PRESENT`, the listener) is too.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// A guard that leaves the process as it found it: Metamod absent, no hooks, no API.
struct Reset;

impl Drop for Reset {
    fn drop(&mut self) {
        // No `assert!` here: a panic while a test is already unwinding aborts the process. Every
        // failure is recorded and `with_plugin` asserts the list once this guard has run.
        MOCK_KHOOK.pop_next.store(false, Ordering::SeqCst);
        MOCK_KHOOK.release_leaked();
        plugin::set_unload_deadline(std::time::Duration::from_secs(10));
        if plugin::is_present() {
            let (object, _) = create_plugin_object();
            if object.is_null() {
                record_violation("cleanup could not get the plugin object".to_owned());
            } else {
                let mut error = [0 as c_char; 256];
                // SAFETY: this crate's plugin object.
                let ok = unsafe { call_unload(object, error.as_mut_ptr(), error.len()) };
                if !ok {
                    record_violation(format!("cleanup could not unload: {}", cstr(&error)));
                }
            }
        }
        if hook::api().is_some() {
            if let Err(drained) = hook::remove_all_blocking(std::time::Duration::from_secs(10)) {
                record_violation(format!("cleanup could not drain the hooks: {drained:?}"));
            }
        }
        hook::uninstall();
        plugin::set_unload_deadline(plugin::DEFAULT_UNLOAD_DEADLINE);
        // The plugin table's process-wide state: whatever a case left (a stored manager, a
        // snapshot) must not leak into the next one.
        metamod::forget();
    }
}

/// Run one case with the process-wide state reset afterwards, and fail it on any mock-contract
/// violation a `extern "C"` mock recorded (see [`VIOLATIONS`]).
fn with_plugin<R>(body: impl FnOnce() -> R) -> R {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        hook::live_context_count(),
        0,
        "a previous case left hook contexts KHook can still call into"
    );
    MOCK_KHOOK.reset();
    MockSmm::lock_of(&MOCK_SMM.detour_ids).clear();
    MockSmm::lock_of(&MOCK_SMM.listeners).clear();
    MOCK_PLMGR.reset();
    metamod::forget();
    EXPECTED_DETOUR_ID.store(MOCK_PLUGIN_ID, Ordering::SeqCst);
    let _ = take_violations(); // nothing from a previous case
    let result = {
        let _reset = Reset;
        body()
    };
    let violations = take_violations();
    assert!(
        violations.is_empty(),
        "the mock APIs recorded contract violations: {violations:#?}"
    );
    result
}

/// The recorder is load-bearing: a failed check must reach the test body, and a passing one
/// must record nothing. (Inside the mocks this is what replaces `assert!`, which would unwind
/// across the C ABI.)
#[test]
fn mock_checks_record_violations_instead_of_panicking() {
    scratch_log();
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = take_violations();
    check(true, || "a passing check records nothing".to_owned());
    check(false, || "a failing check records its message".to_owned());
    assert_eq!(
        take_violations(),
        vec!["a failing check records its message".to_owned()]
    );
}

/// `CreateInterface(METAMOD_PLAPI_NAME, &ret)` as Metamod calls it
/// (`core/metamod_plugins.cpp:571`), through the exported function.
fn create_plugin_object() -> (*mut c_void, c_int) {
    let mut ret: c_int = -1;
    // SAFETY: the name is a valid NUL-terminated literal and `ret` outlives the call.
    let object = unsafe { cs2_bridge::CreateInterface(PLAPI_NAME_C.as_ptr(), &mut ret) };
    (object, ret)
}

/// The `ISmmPlugin` vtable of an object `CreateInterface` returned.
///
/// # Safety
///
/// `object` must be an `ISmmPlugin` this crate handed out.
unsafe fn plugin_vtable(object: *mut c_void) -> *const *const c_void {
    let obj = object.cast::<ISmmPlugin>();
    // SAFETY: the caller's contract; the generated accessor reborrows the vtable.
    let vt: &s2sdk_sys::generated::ISmmPlugin_vtable = unsafe { ISmmPlugin::vtable(obj) };
    (vt as *const s2sdk_sys::generated::ISmmPlugin_vtable).cast::<*const c_void>()
}

/// Call `Load` through the vtable slot Metamod uses (`core/metamod_plugins.cpp:619`).
///
/// # Safety
///
/// `object` must be the plugin object; `ismm` a live `ISmmAPI`.
unsafe fn call_load(
    object: *mut c_void,
    id: c_int,
    ismm: *mut ISmmAPI,
    error: *mut c_char,
    maxlen: usize,
    late: bool,
) -> bool {
    // SAFETY: the caller's contract; the slot is the generated `Load`.
    // the test binary has no -insecure on its command line (cs2_bridge::vac)
    cs2_bridge::vac::allow_secure_for_tests();
    let vt = unsafe { plugin_vtable(object) };
    let load: unsafe extern "C" fn(
        *mut ISmmPlugin,
        c_int,
        *mut ISmmAPI,
        *mut c_char,
        usize,
        bool,
    ) -> bool = unsafe { core::mem::transmute(*vt.add(ISmmPlugin_slots::Load)) };
    // SAFETY: as above.
    unsafe { load(object.cast::<ISmmPlugin>(), id, ismm, error, maxlen, late) }
}

/// Call `Unload` through its slot (`core/metamod_plugins.cpp:656`).
///
/// # Safety
///
/// `object` must be the plugin object.
unsafe fn call_unload(object: *mut c_void, error: *mut c_char, maxlen: usize) -> bool {
    // SAFETY: the caller's contract.
    let vt = unsafe { plugin_vtable(object) };
    let unload: unsafe extern "C" fn(*mut ISmmPlugin, *mut c_char, usize) -> bool =
        unsafe { core::mem::transmute(*vt.add(ISmmPlugin_slots::Unload)) };
    // SAFETY: as above.
    unsafe { unload(object.cast::<ISmmPlugin>(), error, maxlen) }
}

/// Call `QueryRunning` through its slot, the way `meta list` and Metamod's own checks do
/// (`core/metamod_console.cpp:323`, `core/metamod_plugins.cpp:808`).
///
/// # Safety
///
/// `object` must be the plugin object.
unsafe fn call_query_running(object: *mut c_void, error: *mut c_char, maxlen: usize) -> bool {
    // SAFETY: the caller's contract; the slot is the generated `QueryRunning`.
    let vt = unsafe { plugin_vtable(object) };
    let query_running: unsafe extern "C" fn(*mut ISmmPlugin, *mut c_char, usize) -> bool =
        unsafe { core::mem::transmute(*vt.add(ISmmPlugin_slots::QueryRunning)) };
    // SAFETY: as above.
    unsafe { query_running(object.cast::<ISmmPlugin>(), error, maxlen) }
}

/// Call a `const char *f()` slot, for the strings Metamod prints.
///
/// # Safety
///
/// `object` must be the plugin object and `slot` a generated `const char *` entry.
unsafe fn call_string_slot(object: *mut c_void, slot: usize) -> *const c_char {
    // SAFETY: the caller's contract.
    let vt = unsafe { plugin_vtable(object) };
    let f: unsafe extern "C" fn(*mut ISmmPlugin) -> *const c_char =
        unsafe { core::mem::transmute(*vt.add(slot)) };
    // SAFETY: as above.
    unsafe { f(object.cast::<ISmmPlugin>()) }
}

/// Read a C string Metamod would print, for the assertions on the error buffer. Lossy rather
/// than `expect`: this also runs from `Reset::drop`, which must not panic.
fn cstr(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Read the `McpBridge_001` status line through the object's own vtable.
///
/// The line's `"roles"` field is this test binary's role set and not something a case sets:
/// `log::Roles` is per image and only grows. Every case here reaches the object through
/// `CreateInterface("ISmmPlugin")` and then loads it, and both add the `mm` role
/// (`src/lib.rs`, `src/plugin.rs`); [`assert_image_is_metamod`] asserts that precondition
/// first, so a case that asserts the whole line is asserting `metamod`/`khook`/`tick` - never
/// the order the harness happened to run the cases in.
fn status() -> String {
    assert_image_is_metamod();
    iface::status_via_vtable(256).1
}

/// The precondition [`status`] relies on: this binary's image serves the Metamod role only.
///
/// `cs2_bridge::log::Roles` is a per-image set that only grows. Every case here reaches the
/// object through `CreateInterface("ISmmPlugin")` and then loads it, which adds the `mm` role
/// (`plugin::load`, `src/plugin.rs`), and nothing in this binary ever reaches the tools entry
/// point - `toolsys::ticks()` is read here, `toolsys::object()` never is - so the set is `mm`
/// whichever case the harness ran first. The assertion is what makes the `"roles"` field of a
/// status line a stated precondition rather than an accidental one.
fn assert_image_is_metamod() {
    let roles = cs2_bridge::log::roles();
    assert_eq!(
        roles,
        cs2_bridge::log::Roles::NONE.with(cs2_bridge::log::Role::Metamod),
        "this binary's log lines and status lines carry the roles this image serves; every case \
         here is the Metamod plugin role, so the set must be exactly `mm` unless something \
         reached the tools entry point, ToolSystem2_001"
    );
}

// ---------------------------------------------------------------------------------------
// Gate 2: the happy path
// ---------------------------------------------------------------------------------------

/// `CreateInterface("ISmmPlugin")` answers the plugin object with `*ret = META_IFACE_OK`, the
/// object's table is the generated one, and `GetApiVersion` is 18.
#[test]
fn plugin_object_is_served_with_the_generated_vtable() {
    scratch_log();
    with_plugin(|| {
        let (object, ret) = create_plugin_object();
        assert!(!object.is_null(), "ISmmPlugin must be served on this target");
        assert_eq!(ret, plugin::META_IFACE_OK, "*ret = 0 means META_IFACE_OK");

        // The object is the one the crate keeps, and its table is the generated table's size.
        assert_eq!(object as usize, plugin::object() as usize);
        assert_eq!(
            plugin::VT_LEN * core::mem::size_of::<usize>(),
            core::mem::size_of::<s2sdk_sys::generated::ISmmPlugin_vtable>(),
            "the table is the generated vtable, slot for slot"
        );

        // `GetApiVersion` at its generated slot answers the generated 18
        // (`core/ISmmPluginExt.h:69`).
        // SAFETY: the object is this crate's plugin object.
        let vt = unsafe { plugin_vtable(object) };
        // SAFETY: the slot holds `get_api_version` (`*mut ISmmPlugin -> i32`).
        let get_api_version: unsafe extern "C" fn(*mut ISmmPlugin) -> c_int =
            unsafe { core::mem::transmute(*vt.add(ISmmPlugin_slots::GetApiVersion)) };
        // SAFETY: as above.
        assert_eq!(
            unsafe { get_api_version(object.cast::<ISmmPlugin>()) },
            METAMOD_PLAPI_VERSION as c_int
        );
        assert_eq!(METAMOD_PLAPI_VERSION, 18);

        // `GetURL` answers the empty string: this plugin has no site to claim
        // (`core/ISmmPlugin.h:193`).
        // SAFETY: the slot holds `GetURL` (`*mut ISmmPlugin -> *const c_char`) and the object is
        // this crate's plugin object; the returned string is static.
        let url = unsafe { call_string_slot(object, ISmmPlugin_slots::GetURL) };
        // SAFETY: as above.
        assert_eq!(unsafe { core::ffi::CStr::from_ptr(url) }, c"");
    });
}

/// `Load` does the `PLUGIN_SAVEVARS` block and installs the listener: it asks
/// `GetDetourInterface(id)` for the `IKHook`, installs it into `s2sdk::hook`, and calls
/// `AddListener(plugin, listener)`.
#[test]
fn load_installs_khook_and_registers_the_listener() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: the object is this crate's plugin object and the mock API is live.
        let ok = unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        };
        assert!(ok, "Load must succeed: {}", cstr(&error));
        assert_eq!(cstr(&error), "", "a successful Load writes no error");

        // `GetDetourInterface(id)` (`core/ISmmPlugin.h:516`), once, with Load's id.
        assert_eq!(MOCK_SMM.detour_ids(), vec![MOCK_PLUGIN_ID]);
        // `AddListener(this, this)` (`samples/s2_sample_mm/src/plugin.cpp:96`).
        assert_eq!(
            MOCK_SMM.listeners(),
            vec![(plugin::object() as usize, plugin::listener() as usize)]
        );

        // The `IKHook` reached `s2sdk::hook`, and it is the object the mock answered with.
        let installed = hook::api().expect("Load installs the KHook API");
        assert!(
            installed.get_context_ptr().is_null(),
            "no hook is running outside a thunk"
        );
        assert!(!plugin::ismm().is_null());
        assert_eq!(plugin::plugin_id(), MOCK_PLUGIN_ID);
        assert!(plugin::is_present());

        // The status line says so, in one JSON object.
        let line = status();
        assert!(line.contains("\"metamod\":\"present\""), "{line}");
        assert!(line.contains("\"khook\":true"), "{line}");

        // A second `Load` is refused: Metamod never loads a running plugin twice, and a second
        // one would register the listener again. The state stays what the first `Load` left.
        let mut second_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object, live mock API.
        let second = unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID + 1,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                second_error.as_mut_ptr(),
                second_error.len(),
                true,
            )
        };
        assert!(!second, "a second Load must be refused");
        assert!(cstr(&second_error).contains("already loaded"));
        assert_eq!(plugin::plugin_id(), MOCK_PLUGIN_ID, "the first id is kept");
        assert_eq!(
            MOCK_SMM.listeners().len(),
            1,
            "the listener was not registered twice"
        );
    });
}

/// `OnMetamodQuery("McpBridge_001", &ret)` answers our object with `META_IFACE_OK`, and refuses
/// every other name with `META_IFACE_FAILED` - the header's own default body
/// (`core/ISmmPlugin.h:380-388`).
#[test]
fn on_metamod_query_serves_mcp_bridge_001() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object, live mock API.
        assert!(unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        });

        // SAFETY: `listener()` is this crate's static listener, whose table is the generated
        // one; `ISmmAPI::AddListener` was given exactly this pointer.
        let vt = unsafe { IMetamodListener::vtable(plugin::listener()) };
        let on_metamod_query = vt
            .OnMetamodQuery
            .expect("the listener implements OnMetamodQuery");

        let mut ret: c_int = -1;
        // SAFETY: the listener object, a valid name literal and a writable `ret`.
        let answer = unsafe {
            on_metamod_query(
                plugin::listener(),
                iface::IFACE_NAME.as_ptr(),
                &mut ret as *mut c_int,
            )
        };
        assert_eq!(answer as usize, iface::object() as usize, "our object");
        assert_eq!(ret, plugin::META_IFACE_OK);
        assert_eq!(ret, 0, "META_IFACE_OK is 0 (`core/ISmmPlugin.h:54`)");

        // The object the listener answered with is the one `CreateInterface` serves, and its
        // status line is readable through its own vtable.
        let mut ret: c_int = -1;
        // SAFETY: a valid name literal, writable `ret`.
        let direct =
            unsafe { cs2_bridge::CreateInterface(iface::IFACE_NAME.as_ptr(), &mut ret) };
        assert_eq!(direct as usize, answer as usize);
        assert_eq!(ret, plugin::META_IFACE_OK);
        let line = status();
        // The `"roles"` field is this binary's set (the precondition `status` asserts); what
        // this case proves is the live state: present, KHook installed, and the tick from the
        // tool half, which the Metamod path never drives.
        // v2's members keep their bytes; v3 adds the tick source, the GameFrame counter and the
        // hook's state (this binary's mock ISmmAPI has no factory slots, so it says why).
        assert!(
            line.starts_with(&format!(
                "{{\"metamod\":\"present\",\"khook\":true,\"tick\":{},\"roles\":[\"mm\"],\"tickSource\":\"none\",\"gameframeTicks\":{},",
                toolsys::ticks(),
                cs2_bridge::gameframe::ticks()
            )),
            "{line}"
        );
        assert!(
            line.contains("\"gameframe\":\"unavailable: ISmmAPI has no GetServerFactory/VInterfaceMatch\""),
            "{line}"
        );

        // A name that is not ours is refused, with META_IFACE_FAILED.
        let mut ret: c_int = -1;
        // SAFETY: as above; the name is Metamod's own plugin-manager interface, which this
        // listener does not implement.
        let answer = unsafe { on_metamod_query(plugin::listener(), c"ISmmPluginManager".as_ptr(), &mut ret as *mut c_int) };
        assert!(answer.is_null());
        assert_eq!(ret, plugin::META_IFACE_FAILED);
        assert_eq!(ret, 1, "META_IFACE_FAILED is 1 (`core/ISmmPlugin.h:55`)");

        // And a null `ret` is allowed: Metamod passes one when it only wants the object
        // (`core/metamod.cpp:830-846` checks `if (ret)`).
        // SAFETY: as above, with a null `ret`.
        let answer =
            unsafe { on_metamod_query(plugin::listener(), iface::IFACE_NAME.as_ptr(), ptr::null_mut()) };
        assert_eq!(answer as usize, iface::object() as usize);
    });
}

/// The whole lifecycle: load, hook, drop the hook, unload. `Unload` removes the hook through
/// KHook (`remove_all_blocking` re-issues the drop's queued removal synchronously), then the
/// teardown runs and Metamod is "absent" again.
#[test]
fn unload_drains_hooks_then_tears_down() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object, live mock API.
        assert!(unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        });

        // One hook, registered through the wrapper (`IKHook::SetupHook` -> the mock).
        // SAFETY: `hooked_void` is a live `void f(this)` function and the thunks match it.
        let hook = unsafe {
            h_void::setup_inline(
                hooked_void as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        assert_eq!(MOCK_KHOOK.held_len(), 1, "the mock holds the hook");
        let setups = MOCK_KHOOK.setups();
        assert_eq!(setups.len(), 1, "one SetupHook call");
        assert!(
            setups[0].r#async,
            "the wrapper's async flag must reach KHook verbatim"
        );
        assert!(setups[0].stack_size > 0, "the copy stack is the wrapper's");
        assert_eq!(hook::live_context_count(), 1);

        // Dropping it queues an asynchronous removal, which KHook's delete thread would run
        // later (`detour.cpp:1852-1867`); the mock's thread never runs, so only the drain can
        // finish it - which is exactly the case `Unload` has to handle.
        drop(hook);
        assert_eq!(MOCK_KHOOK.held_len(), 1, "the async removal is only queued");
        assert_eq!(hook::live_context_count(), 1);

        let mut unload_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        let ok = unsafe { call_unload(object, unload_error.as_mut_ptr(), unload_error.len()) };
        assert!(ok, "Unload must succeed: {}", cstr(&unload_error));
        assert_eq!(cstr(&unload_error), "");

        // The drain re-issued the removal synchronously and the mock ran the removal function;
        // its final barrier names `INVALID_HOOK` (`khook.hpp:116`), which is no hook at all.
        //
        // The drain also re-issues every id a case of this binary once *lost* to KHook's insert
        // thread (`remove_all_blocking`: "the ids are permanent, so every drain re-issues all of
        // them"), so a case that ran after the lost-removal one sees those removals here too.
        // This case's own sequence is the first two and the barrier last.
        let removals = MOCK_KHOOK.removals();
        // Exactly this case's three (the Drop's asynchronous removal, the drain's re-issue and
        // the barrier) plus one re-issue per id an earlier case lost, which the drain re-issues
        // because ids are permanent.
        assert_eq!(removals.len(), 3 + hook::lost_removal_count(), "{removals:?}");
        assert!(removals[0].r#async, "the Drop's own removal");
        assert!(!removals[1].r#async, "the drain's re-issue");
        assert_eq!(removals[1].id, removals[0].id, "same hook");
        let barrier = removals.last().expect("the drain's barrier");
        assert_eq!(
            barrier.id,
            hook::INVALID_HOOK,
            "the drain's barrier removal is synchronous and names no hook: {removals:?}"
        );
        assert!(!barrier.r#async);
        assert_eq!(MOCK_KHOOK.held_len(), 0, "KHook no longer holds it");
        assert_eq!(hook::live_context_count(), 0);

        // The teardown: Metamod absent, the API gone.
        assert!(!plugin::is_present());
        assert!(!plugin::listener_registered());
        assert!(plugin::ismm().is_null());
        assert!(hook::api().is_none(), "Unload forgets the KHook API");
        // The `"roles"` field is this binary's set (the precondition `status` asserts) and
        // stays so: tearing the Metamod state down does not remove the role.
        let line = status();
        assert!(
            line.starts_with(&format!(
                "{{\"metamod\":\"absent\",\"khook\":false,\"tick\":{},\"roles\":[\"mm\"],\"tickSource\":\"none\",\"gameframeTicks\":{},",
                toolsys::ticks(),
                cs2_bridge::gameframe::ticks()
            )),
            "{line}"
        );
    });
}

// ---------------------------------------------------------------------------------------
// Gate 2b: the setup mode is forwarded, not normalised
// ---------------------------------------------------------------------------------------

/// `SetupMode::Sync` reaches KHook as `async = false` and `SetupMode::Async` as `true`: the
/// flag is what KHook uses to decide whether the insert may take the hooked address's mutex
/// inside `SetupHook` (`khook.hpp:2186`, `detour.cpp:1738-1748`), so the wrapper must pass it
/// through instead of choosing one.
#[test]
fn setup_mode_reaches_khook_as_the_async_flag() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object, live mock API.
        assert!(unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        });

        // SAFETY: `hooked_void` is a live `void f(this)` function and the thunks match it.
        let sync = unsafe {
            h_void::setup_inline(
                hooked_void as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Sync,
            )
        }
        .expect("the mock accepts the registration");
        let setups = MOCK_KHOOK.setups();
        assert_eq!(setups.len(), 1, "one SetupHook call");
        assert!(
            !setups[0].r#async,
            "SetupMode::Sync must arrive as async = false"
        );
        assert_eq!(hook::live_context_count(), 1);
        drop(sync);

        // SAFETY: as above.
        let asynchronous = unsafe {
            h_void::setup_inline(
                hooked_void as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        let setups = MOCK_KHOOK.setups();
        assert_eq!(setups.len(), 2, "two SetupHook calls");
        assert!(
            setups[1].r#async,
            "SetupMode::Async must arrive as async = true"
        );
        assert_eq!(hook::live_context_count(), 2);
        drop(asynchronous);
    });
}

// ---------------------------------------------------------------------------------------
// Gate 3: the negative cases
// ---------------------------------------------------------------------------------------

/// (a) An unknown interface name is refused: NULL and `*ret = META_IFACE_FAILED`.
#[test]
fn unknown_interface_name_is_refused() {
    scratch_log();
    with_plugin(|| {
        let mut ret: c_int = -1;
        // SAFETY: a valid name literal and a writable `ret`.
        let object = unsafe { cs2_bridge::CreateInterface(c"Not_An_Interface".as_ptr(), &mut ret) };
        assert!(object.is_null());
        assert_eq!(ret, 1);
        assert_eq!(ret, plugin::META_IFACE_FAILED);
    });
}

/// (b) `Load` with a null `ismm` refuses the load with the error filled and does not crash -
/// whatever `late` says.
#[test]
fn load_with_null_ismm_refuses_with_the_error_filled() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();
        for late in [false, true] {
            let mut error = [0 as c_char; 256];
            // SAFETY: this crate's plugin object; a null `ismm` is the case under test.
            let ok = unsafe {
                call_load(object, MOCK_PLUGIN_ID, ptr::null_mut(), error.as_mut_ptr(), error.len(), late)
            };
            assert!(!ok, "Load must refuse a null ISmmAPI (late = {late})");
            let message = cstr(&error);
            assert!(!message.is_empty(), "the error buffer must be filled");
            assert!(message.contains("null ISmmAPI"), "{message}");
            assert!(!plugin::is_present(), "a refused load leaves Metamod absent");
            assert!(hook::api().is_none(), "a refused load installs no API");
            assert_eq!(MOCK_SMM.detour_ids(), Vec::<c_int>::new());
        }
    });
}

/// (b, second half) A `Load` whose `GetDetourInterface` answers null refuses too: without KHook
/// this plugin cannot do its job, and a half-loaded plugin is worse than a refused one.
#[test]
fn load_without_khook_refuses() {
    scratch_log();
    with_plugin(|| {
        // A second `ISmmAPI`, complete as far as this plugin is concerned, whose
        // `GetDetourInterface` answers null (a Metamod without KHook).
        static NO_KHOOK_VTABLE: SmmVtable = SmmVtable(ISmmAPI_vtable {
            AddListener: Some(no_khook_add_listener),
            GetDetourInterface: Some(no_khook_get_detour_interface),
            ..ISmmAPI_vtable::NULL
        });
        struct NoKhookObj(ISmmAPI);

        // SAFETY: as for `SmmObj` - a pointer to the immutable static table above.
        unsafe impl Sync for NoKhookObj {}

        static NO_KHOOK_OBJ: NoKhookObj = NoKhookObj(ISmmAPI {
            vtable: ptr::addr_of!(NO_KHOOK_VTABLE.0).cast::<c_void>(),
        });

        /// `ISmmAPI::GetDetourInterface` (`core/ISmmAPI.h:449`) with no KHook to give.
        unsafe extern "C" fn no_khook_get_detour_interface(
            _this: *mut ISmmAPI,
            id: c_int,
        ) -> *mut c_void {
            check(id == MOCK_PLUGIN_ID, || {
                format!("no-khook GetDetourInterface with plugin id {id}")
            });
            ptr::null_mut()
        }

        /// `ISmmAPI::AddListener` (`core/ISmmAPI.h:201`): present in the table, but a `Load`
        /// that has no KHook must return before it ever calls this.
        unsafe extern "C" fn no_khook_add_listener(
            _this: *mut ISmmAPI,
            _plugin: *mut ISmmPlugin,
            _listener: *mut IMetamodListener,
        ) {
            record_violation(
                "AddListener must not run when GetDetourInterface answered null".to_owned(),
            );
        }

        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object and the static mock API above.
        let ok = unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(NO_KHOOK_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        };
        assert!(!ok);
        let message = cstr(&error);
        assert!(message.contains("GetDetourInterface returned null"), "{message}");
        assert!(!plugin::is_present());
        assert!(hook::api().is_none());
    });
}

/// A removal KHook *loses* to its insert-thread window is reconciled, not waited out: `Unload`
/// completes and the id is reported.
///
/// The state is the one real KHook produces (`detour.cpp:1607-1613` against
/// `:1508-1543,1566-1571,1582-1589`): the insert thread has popped this registration's
/// `(id, details)` and not applied them yet, so the id is associated with no capsule callback. A
/// synchronous `RemoveHook` then finds the id associated, finds no callback, answers "gone"
/// and **never runs the removal function**, and the insert thread drops the details. That is the
/// only way a synchronous removal returns without running it — a *blocked* removal does not
/// return at all (`__RemoveHook_Sync(id, true)` loops on the capsule until it owns it,
/// `detour.cpp:1579-1597`), so `UnloadTimeout` is not reachable from a faithful KHook at all (the
/// crate's `unload_timeout_backstop` unit test covers the defensive path).
///
/// `remove_all_blocking` sees the id still tracked after its synchronous call and reconciles it
/// (`s2sdk::hook::reconcile_lost`): the count comes down, the context is leaked on purpose, and
/// the unload is allowed. After that the removal function may still be called by Metamod's
/// per-plugin unloader (`~CPlugin` builds it and issues `RemoveHook` for every id the plugin ever
/// registered, `core/metamod_plugins.cpp:46-56,205-213`) with the API gone; it must ignore the
/// reconciled id. The harness runs it below: before item 1 that call was a panic in `extern "C"`
/// and aborted the process.
#[test]
fn unload_reconciles_a_removal_khook_lost_to_the_insert_thread() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object, live mock API.
        assert!(unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        });

        // Arm the insert-thread window for this registration, then register.
        MOCK_KHOOK.pop_next.store(true, Ordering::SeqCst);
        // SAFETY: `hooked_void` is a live `void f(this)` function and the thunks match it.
        let hook = unsafe {
            h_void::setup_inline(
                hooked_void as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        let id = hook.id();
        assert_eq!(MOCK_KHOOK.held_len(), 0, "the capsule has no callback for it");
        assert_eq!(MOCK_KHOOK.window_len(), 1, "the insert thread holds the entry");
        assert_eq!(MOCK_KHOOK.window_ids(), vec![id], "it is this registration");
        drop(hook);
        assert_eq!(hook::live_context_count(), 1);
        assert_eq!(MOCK_KHOOK.leaked_len(), 0, "nothing is lost yet");

        // The unload's drain re-issues the pending removal synchronously; KHook answers "gone",
        // so the id is reconciled instead of waited for.
        plugin::set_unload_deadline(std::time::Duration::from_millis(50));
        let lost_before = hook::lost_removal_count();
        let mut unload_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        let ok = unsafe { call_unload(object, unload_error.as_mut_ptr(), unload_error.len()) };
        assert!(
            ok,
            "a lost removal is reconciled, not refused: {}",
            cstr(&unload_error)
        );
        assert_eq!(
            hook::lost_removal_count(),
            lost_before + 1,
            "the id was reconciled as lost"
        );
        assert_eq!(
            hook::live_context_count(),
            0,
            "so nothing is left for KHook to call into"
        );
        assert_eq!(MOCK_KHOOK.window_len(), 0, "the removal erased the association");
        assert_eq!(
            MOCK_KHOOK.leaked_len(),
            1,
            "and no removal function will ever run for it"
        );
        assert!(!plugin::is_present(), "the plugin unloaded");

        // Metamod's unloader, after the API is gone: run the leaked entry's removal function the
        // way the scan does. The reconciled id must be ignored (no double free, no panic).
        MOCK_KHOOK.release_leaked();
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(
            hook::lost_removal_count(),
            lost_before + 1,
            "and no second reconcile"
        );
    });
}

/// `Unload` reports a missing KHook API in its own words, not with the timeout prefix: Metamod
/// ignores the false either way (`Unload(...) || force`, `core/metamod_plugins.cpp:656`), but
/// the message an operator reads must not claim a timeout that did not happen.
#[test]
fn unload_reports_a_missing_khook_api_without_the_timeout_prefix() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object, live mock API.
        assert!(unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        });

        // While loaded, `QueryRunning` answers true with no reason written.
        let mut run_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        assert!(unsafe { call_query_running(object, run_error.as_mut_ptr(), run_error.len()) });
        assert_eq!(cstr(&run_error), "", "a running plugin writes no reason");

        // One hook, dropped: its removal is queued for KHook's delete thread and KHook still
        // holds it, so there is something `Unload` has to remove.
        // SAFETY: `hooked_void` is a live `void f(this)` function and the thunks match it.
        let hook = unsafe {
            h_void::setup_inline(
                hooked_void as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        drop(hook);
        assert_eq!(hook::live_context_count(), 1);

        // The API disappears under the plugin. Only a probe can do this - Metamod keeps the
        // `IKHook` alive for the plugin's lifetime - and it is exactly the state
        // `Error::KHookNotInstalled` describes: work left to do, no API to do it with.
        hook::uninstall();
        let mut unload_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        assert!(!unsafe { call_unload(object, unload_error.as_mut_ptr(), unload_error.len()) });
        let message = cstr(&unload_error);
        assert!(!message.starts_with("UnloadTimeout:"), "{message}");
        assert!(
            message.contains("no KHook IKHook API is installed"),
            "{message}"
        );
        assert!(plugin::is_present(), "nothing was torn down");

        // The registry's record of the id is gone (`remove_all_blocking` takes both sets before
        // it checks the API), so a retry cannot re-issue anything: the context stops being live
        // only when its removal function runs, and that is `h_void::removed`, which asks KHook
        // for its own context (`api_expect().get_context_ptr()`, `s2sdk/src/hook.rs:2464-2469`).
        // Put the adapter back first - KHook would only ever call it while its own `IKHook`
        // exists - then run it, the harness standing in for KHook. The process is back to zero
        // live contexts afterwards, which is what the rest of this binary requires.
        // SAFETY: the mock `IKHook`, live for the process; the adapter is this crate's static.
        unsafe { cs2_bridge::khook_api::install(ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut()) };
        assert_eq!(hook::live_context_count(), 1);
        MOCK_KHOOK.release_held();
        assert_eq!(hook::live_context_count(), 0);
    });
}

/// The `meta retry` sequence of `core/metamod_plugins.cpp:351-398`, driven through the tables.
///
/// One file, one image, one static `ISmmPlugin`: `Retry` `_Load`s the same file (`:364`) while the
/// `CPlugin` whose load failed is still in Metamod's list (`:359-363` allows exactly that state),
/// `dlmount` refcounts the image so the new `CPlugin` resolves *this* object (`:570`), and the
/// retry then calls `_Unload(old, force = true)` (`:372`), which reaches this object's `Unload`
/// (`:656`) and tears the new copy's state down while Metamod keeps the new `CPlugin` as
/// `Pl_Running`.
///
/// What this case proves, in the order the sequence happens:
///
/// * the retry's `Load` succeeds on the object the failed one left behind, with the *new* id
///   (`GetDetourInterface(new id)`, `:619`, `core/ISmmPlugin.h:516`);
/// * the superseded copy's forced `Unload` answers true and drains through the `IKHook` the new
///   load installed (the mock's table checks `this` on every call), leaving no live context;
/// * the object is then still usable - the same `Load` a fresh `CPlugin` would make succeeds - so
///   the teardown left nothing behind that would make a later load refuse. In Metamod that later
///   load normally follows the image being unmapped and mapped again (the retry's `CPlugin`
///   deletion `dlclose`s it, `:205-214`, `:71`), which re-runs the statics; this proves the
///   in-image half of it.
///
/// The mismatch the sequence leaves - Metamod says `Pl_Running`, the status line says absent - is
/// documented in `plugin.rs`'s retry section along with why no plugin-side guard exists: the
/// `Unload` call cannot be attributed to a `CPlugin`.
#[test]
fn retry_loads_the_same_object_and_the_superseded_unload_tears_the_new_copy_down() {
    scratch_log();
    with_plugin(|| {
        let (object, _) = create_plugin_object();

        // 1. The copy whose `Load` failed. Any false leaves Metamod with a `Pl_Refused` plugin
        //    (`core/metamod_plugins.cpp:627-630`) that it keeps in its list, and `Retry` acts
        //    only on a status below `Pl_Paused` (`:359-363`); a null `ISmmAPI` is the refusal
        //    that needs no second mock.
        let mut refused_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object; a null `ismm` is the refusal under test.
        let refused = unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::null_mut(),
                refused_error.as_mut_ptr(),
                refused_error.len(),
                false,
            )
        };
        assert!(!refused, "the first load is the refused one");
        assert!(cstr(&refused_error).contains("null ISmmAPI"));
        assert!(!plugin::is_present());

        // 2. The retry: the same object, the same `ISmmAPI`, a *new* id - what `_Load` does at
        //    `:619` with the id it assigned at `:461-464`.
        let retry_id = MOCK_PLUGIN_ID + 1;
        EXPECTED_DETOUR_ID.store(retry_id, Ordering::SeqCst);
        let mut retry_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object and the live mock API.
        let retried = unsafe {
            call_load(
                object,
                retry_id,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                retry_error.as_mut_ptr(),
                retry_error.len(),
                false,
            )
        };
        assert!(retried, "the retry's Load must succeed: {}", cstr(&retry_error));
        assert_eq!(MOCK_SMM.detour_ids(), vec![retry_id]);
        assert_eq!(plugin::plugin_id(), retry_id);
        assert!(plugin::is_present());
        assert!(hook::api().is_some(), "the retry installed the KHook");

        // A hook under the new copy: the drain of step 3 has to reach it through the `IKHook`
        // the *new* load installed (the mock records a violation for any other object).
        // SAFETY: `hooked_void` is a live `void f(this)` function and the thunks match it.
        let hook = unsafe {
            h_void::setup_inline(
                hooked_void as *mut c_void,
                Vec::new(),
                Vec::new(),
                None,
                SetupMode::Async,
            )
        }
        .expect("the mock accepts the registration");
        drop(hook);
        assert_eq!(hook::live_context_count(), 1);

        // 3. `_Unload` of the superseded copy with `force = true` (`:372`): this object's
        //    `Unload` is called for a `CPlugin` that is not the one it is live under.
        let mut unload_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        let ok = unsafe { call_unload(object, unload_error.as_mut_ptr(), unload_error.len()) };
        assert!(ok, "the forced unload of the superseded copy: {}", cstr(&unload_error));
        assert_eq!(cstr(&unload_error), "");

        // Torn down, and nothing left that KHook could call back into.
        assert_eq!(hook::live_context_count(), 0);
        assert_eq!(MOCK_KHOOK.held_len(), 0);
        // The drain re-issues the ids of lost removals from earlier cases too (they are
        // permanent, `remove_all_blocking`'s doc): this case's sequence is the first two and the
        // barrier last.
        let removals = MOCK_KHOOK.removals();
        // Exactly this case's three (the Drop's asynchronous removal, the drain's re-issue and
        // the barrier) plus one re-issue per id an earlier case lost, which the drain re-issues
        // because ids are permanent.
        assert_eq!(removals.len(), 3 + hook::lost_removal_count(), "{removals:?}");
        assert!(removals[0].r#async, "the Drop's own removal");
        assert!(!removals[1].r#async, "the drain's re-issue");
        let barrier = removals.last().expect("the drain's barrier");
        assert!(!barrier.r#async, "the drain's barrier");
        assert_eq!(barrier.id, hook::INVALID_HOOK, "{removals:?}");
        assert!(!plugin::is_present());
        assert!(plugin::ismm().is_null());
        assert!(!plugin::listener_registered());
        assert!(hook::api().is_none());
        let line = status();
        assert!(line.contains("\"metamod\":\"absent\""), "{line}");
        assert!(line.contains("\"khook\":false"), "{line}");
        assert_eq!(
            MOCK_SMM.listeners().len(),
            1,
            "only the load that got past GetDetourInterface registers the listener; the refused \
             one returned long before it"
        );

        // 4. The object still takes a load - the recovery path's in-image half.
        EXPECTED_DETOUR_ID.store(MOCK_PLUGIN_ID, Ordering::SeqCst);
        let mut again_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object and the live mock API.
        let again = unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                again_error.as_mut_ptr(),
                again_error.len(),
                false,
            )
        };
        assert!(
            again,
            "the object takes a load after the sequence: {}",
            cstr(&again_error)
        );
        assert!(plugin::is_present());
        assert_eq!(plugin::plugin_id(), MOCK_PLUGIN_ID);
        assert!(hook::api().is_some());
    });
}

/// Metamod calls `Unload` even for a plugin whose `Load` returned false: `_Unload` checks only
/// `m_API && m_Lib` (`core/metamod_plugins.cpp:653`), and `UnloadAll` runs it for every plugin
/// at shutdown and on `meta clear`. That must touch nothing - in particular it must not run the
/// tool object's shutdown sequence, which would tear down the IPC of a live tool object.
#[test]
fn unload_without_a_successful_load_touches_nothing() {
    scratch_log();
    with_plugin(|| {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch/cs2-bridge-unload-not-loaded.log");
        let _ = std::fs::remove_file(&path);
        std::env::set_var("CS2_BRIDGE_LOG", &path);

        let (object, _) = create_plugin_object();
        // A `Load` that was refused (no ISmmAPI) leaves the plugin not present.
        let mut load_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object; a null `ismm` is the case.
        assert!(!unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::null_mut(),
                load_error.as_mut_ptr(),
                load_error.len(),
                false,
            )
        });
        assert!(!plugin::is_present());

        // `meta list` and Metamod's own checks ask QueryRunning
        // (`core/metamod_console.cpp:323`, `core/metamod_plugins.cpp:808`); with no Load
        // completed the answer is false and the buffer names the reason.
        let mut run_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        assert!(!unsafe { call_query_running(object, run_error.as_mut_ptr(), run_error.len()) });
        assert!(
            cstr(&run_error).contains("no Metamod Load has completed"),
            "{}",
            cstr(&run_error)
        );

        let mut unload_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        let ok = unsafe { call_unload(object, unload_error.as_mut_ptr(), unload_error.len()) };
        // Restore the binary's own scratch log: the next case's lines must not land in
        // the user's real bridge log either.
        std::env::set_var("CS2_BRIDGE_LOG", scratch_log());

        assert!(ok, "an unload of a plugin that never loaded is a no-op success");
        assert_eq!(cstr(&unload_error), "", "and writes no error");
        assert!(!plugin::is_present());
        assert!(hook::api().is_none());

        let text = std::fs::read_to_string(&path).expect("the log file must exist");
        assert!(
            text.contains("Unload: not loaded here; nothing to do"),
            "{text}"
        );
        // The refused load's own lines carry the Metamod role tag: the entry point added the
        // role before `Load` ran (`src/lib.rs`), so a live log tells the roles of the image
        // apart even for a plugin Metamod refused.
        assert!(text.contains("[cs2-bridge mm] Load(id="), "{text}");
        assert!(
            text.contains("[cs2-bridge mm] cs2-mcp bridge: Load got a null ISmmAPI*"),
            "{text}"
        );
        assert!(
            !text.contains("[cs2-bridge none]"),
            "no line of this image is tagged with the unnamed image: {text}"
        );
        assert!(
            !text.contains("Shutdown"),
            "the tool object's shutdown sequence must not run: {text}"
        );
        let _ = std::fs::remove_file(&path);
    });
}

/// `Unload` tears down only what `Load` started: the KHook adapter and the listener state.
/// The tool object's shutdown sequence (`m0_Shutdown` -> `m0_PreShutdown` -> "Shutdown done",
/// `mcp_bridge.c:6783-6792`) belongs to that object's own `Shutdown` slot and must not run
/// here - in a process that serves both objects it would tear down the live tool half.
#[test]
fn unload_tears_down_only_what_load_started() {
    scratch_log();
    with_plugin(|| {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch/cs2-bridge-unload-order.log");
        let _ = std::fs::remove_file(&path);
        std::env::set_var("CS2_BRIDGE_LOG", &path);

        let (object, _) = create_plugin_object();
        let mut error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object, live mock API.
        assert!(unsafe {
            call_load(
                object,
                MOCK_PLUGIN_ID,
                ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
                error.as_mut_ptr(),
                error.len(),
                false,
            )
        });
        let mut unload_error = [0 as c_char; 256];
        // SAFETY: this crate's plugin object.
        assert!(unsafe { call_unload(object, unload_error.as_mut_ptr(), unload_error.len()) });
        // Restore the binary's own scratch log: the next case's lines must not land in
        // the user's real bridge log either.
        std::env::set_var("CS2_BRIDGE_LOG", scratch_log());

        let text = std::fs::read_to_string(&path).expect("the log file must exist");
        let tail: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.ends_with("Unload"))
            .collect();
        assert_eq!(
            tail,
            vec![
                "[cs2-bridge mm] Unload",
                // The hook owners' teardown runs first, one line per owner (`crate::hooks`):
                // the GameFrame tick (`gameframe.rs`), then the C's order - input, the sound
                // wrap, the sound listener - then save/restore.
                "[cs2-bridge mm] hooks teardown: gameframe: not armed (no-op)",
                "[cs2-bridge mm] hooks teardown: input: not armed (no-op)",
                "[cs2-bridge mm] hooks teardown: sound: not armed (no-op)",
                "[cs2-bridge mm] hooks teardown: sound listener: not registered (no-op)",
                "[cs2-bridge mm] hooks teardown: save/restore: not armed (no-op)",
                "[cs2-bridge mm] khook: IKHook uninstalled",
                "[cs2-bridge mm] Unload: hooks removed, KHook forgotten, Metamod absent",
            ],
            "only the Metamod state is torn down"
        );
        assert!(
            !text.contains("Shutdown"),
            "the tool object's shutdown sequence is its own slot's job: {text}"
        );
        let _ = std::fs::remove_file(&path);
    });
}

// ---------------------------------------------------------------------------------------
// Gate B4: the plugin table (`metamod.plugins`)
// ---------------------------------------------------------------------------------------

/// Load the plugin object the way Metamod does (id [`MOCK_PLUGIN_ID`]) - the common setup of
/// every plugin-table case below.
fn load_object() -> *mut c_void {
    let (object, _) = create_plugin_object();
    let mut error = [0 as c_char; 256];
    // SAFETY: the object is this crate's plugin object and the mock API is live.
    let ok = unsafe {
        call_load(
            object,
            MOCK_PLUGIN_ID,
            ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
            error.as_mut_ptr(),
            error.len(),
            false,
        )
    };
    assert!(ok, "Load must succeed: {}", cstr(&error));
    object
}

/// Call `AllPluginsLoaded` through its generated slot (`core/metamod_plugins.cpp:249` fires
/// the batch's `OnPluginLoad`s before it).
///
/// # Safety
///
/// `object` must be the plugin object.
unsafe fn call_all_plugins_loaded(object: *mut c_void) {
    // SAFETY: the caller's contract.
    let vt = unsafe { plugin_vtable(object) };
    let f: unsafe extern "C" fn(*mut ISmmPlugin) = unsafe {
        core::mem::transmute(*vt.add(ISmmPlugin_slots::AllPluginsLoaded))
    };
    // SAFETY: as above.
    unsafe { f(object.cast::<ISmmPlugin>()) }
}

/// Call the listener's `OnPluginLoad`/`OnPluginUnload` through their generated slots, the way
/// Metamod fires the events (`core/metamod_plugins.cpp:249`, `:345`).
///
/// # Safety
///
/// `plugin::listener()` is this crate's static listener.
unsafe fn call_listener_event(id: c_int, unload: bool) {
    // SAFETY: the static listener, whose table is the generated one.
    let vt = unsafe { IMetamodListener::vtable(plugin::listener()) };
    let entry = if unload { vt.OnPluginUnload } else { vt.OnPluginLoad };
    let f = entry.expect("the listener implements the plugin events");
    // SAFETY: the listener object and an id Metamod would pass.
    unsafe { f(plugin::listener(), id) }
}

/// Call the listener's `OnPluginPause`/`OnPluginUnpause` through their generated slots, the
/// way Metamod fires them after `_Pause`/`_Unpause` set the status
/// (`core/metamod_plugins.cpp:303-305`, `:323-325`).
///
/// # Safety
///
/// `plugin::listener()` is this crate's static listener.
unsafe fn call_pause_event(id: c_int, unpause: bool) {
    // SAFETY: the static listener, whose table is the generated one.
    let vt = unsafe { IMetamodListener::vtable(plugin::listener()) };
    let entry = if unpause { vt.OnPluginUnpause } else { vt.OnPluginPause };
    let f = entry.expect("the listener implements the plugin events");
    // SAFETY: the listener object and an id Metamod would pass.
    unsafe { f(plugin::listener(), id) }
}

/// A row for the mock manager, spelled the way the cases read.
fn row(id: c_int, file: &'static str, status: Pl_Status, source: c_int) -> MockPlugin {
    MockPlugin { id, file, status, source }
}

/// `AllPluginsLoaded` builds the table: ids found are rows in ascending order, the gap (id 2,
/// a plugin unloaded earlier - `Query` answers false for it) is skipped, our own id carries
/// `self:true`, and every `file` was COPIED (the mock's answer buffer is rewritten after the
/// rebuild and the rows do not move).
#[test]
fn all_plugins_loaded_builds_the_plugin_table() {
    scratch_log();
    with_plugin(|| {
        // Every mock row's `source` cites a real load path: a tools-vdf/ini load is
        // `Pl_File` (-2) (`core/metamod.cpp:326`, `:1182`), a console `meta load` is
        // `Pl_Console` (-1) (`core/metamod_console.cpp:428`).
        MOCK_PLMGR.set_plugins(&[
            row(1, "mm_plugin_one.dll", Pl_Status::Pl_Running, -1), // Pl_Console
            // id 2: deliberately absent - the gap an unload left.
            row(3, "mm_plugin_three.dll", Pl_Status::Pl_Paused, -2), // Pl_File
            row(MOCK_PLUGIN_ID, "self-plugin.dll", Pl_Status::Pl_Running, -2),
        ]);
        let object = load_object();

        // `Load` itself rebuilt once (the late-load snapshot, `plugin::load`'s contract);
        // `AllPluginsLoaded` is the settled batch's rebuild.
        // SAFETY: this crate's plugin object.
        unsafe { call_all_plugins_loaded(object) };
        let snap = metamod::current_snapshot().expect("AllPluginsLoaded published a snapshot");
        let ids: Vec<i32> = snap.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 3, MOCK_PLUGIN_ID], "ascending, gap skipped");
        assert_eq!(snap.generation, 2, "Load's rebuild + AllPluginsLoaded's");

        let one = &snap.rows[0];
        assert_eq!((one.file.as_str(), one.status.as_str(), one.source), ("mm_plugin_one.dll", "running", -1));
        let three = &snap.rows[1];
        assert_eq!((three.file.as_str(), three.status.as_str(), three.source), ("mm_plugin_three.dll", "paused", -2));
        let own = &snap.rows[2];
        assert!(own.is_self, "our id is flagged");
        assert!(!one.is_self && !three.is_self);

        // The copy rule: Metamod owns the string. Rewrite the mock's answer buffer - the
        // pointer `Query` handed out now points here - and the published rows must not move.
        let before = snap.rows.clone();
        MOCK_PLMGR.corrupt_file_buf('x' as c_char);
        let after = metamod::current_snapshot().expect("still published").rows;
        assert_eq!(before, after, "the rows own their strings, not Metamod's pointer");
    });
}

/// `OnPluginLoad`/`OnPluginUnload` rebuild the snapshot, and every rebuild that had a manager
/// to query bumps the generation.
#[test]
fn plugin_events_rebuild_and_bump_the_generation() {
    scratch_log();
    with_plugin(|| {
        let _object = load_object();
        let before = metamod::current_snapshot().expect("Load published a snapshot").generation;

        // A plugin joins: the mock grows the row (Metamod's list did) and the event fires
        // (a console `meta load`, `Pl_Console`).
        MOCK_PLMGR.set_plugins(&[row(9, "late_joiner.dll", Pl_Status::Pl_Running, -1)]);
        // SAFETY: this crate's static listener.
        unsafe { call_listener_event(9, false) };
        let snap = metamod::current_snapshot().expect("rebuilt");
        assert_eq!(snap.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![9]);
        assert_eq!(snap.generation, before + 1);

        // ... and leaves: the mock drops the row (Metamod's list did; the old id is what the
        // event carries, `core/metamod_plugins.cpp:345`).
        MOCK_PLMGR.set_plugins(&[]);
        // SAFETY: as above.
        unsafe { call_listener_event(9, true) };
        let snap = metamod::current_snapshot().expect("rebuilt");
        assert_eq!(snap.rows, Vec::new(), "the unloaded plugin is gone");
        assert_eq!(snap.generation, before + 2);

        // The scan bound stayed lifted over the departed id (`note_event_id`), which is why
        // the id can be REUSED by a retry and still be found.
        MOCK_PLMGR.set_plugins(&[row(9, "reused_id.dll", Pl_Status::Pl_Running, -1)]);
        // SAFETY: as above.
        unsafe { call_listener_event(9, false) };
        let snap = metamod::current_snapshot().expect("rebuilt");
        assert_eq!(snap.rows[0].file, "reused_id.dll");

        // The bound is also LIFTED by an event: an id far past our own (7) plus the scan
        // headroom is still found, because `note_event_id(40)` widened the scan
        // (`ops::metamod`'s id bound) - this case FAILS if the rebuild ignores the event
        // ids (the negative below drops `note_event_id` and this assert goes red).
        MOCK_PLMGR.set_plugins(&[
            row(40, "far_late_joiner.dll", Pl_Status::Pl_Running, -2),
            row(MOCK_PLUGIN_ID, "self-plugin.dll", Pl_Status::Pl_Running, -2),
        ]);
        // SAFETY: as above.
        unsafe { call_listener_event(40, false) };
        let snap = metamod::current_snapshot().expect("rebuilt");
        assert_eq!(
            snap.rows.iter().map(|r| r.file.as_str()).collect::<Vec<_>>(),
            vec!["self-plugin.dll", "far_late_joiner.dll"],
            "id 40 is past our id + the headroom; only the event's id reaches it (rows are id-ascending)"
        );
    });
}

/// A status value the generated `Pl_*` consts do not name is reported as `unknown(<n>)`,
/// never guessed into a known name.
#[test]
fn an_unknown_status_is_reported_not_guessed() {
    scratch_log();
    with_plugin(|| {
        MOCK_PLMGR.set_plugins(&[row(4, "odd.dll", Pl_Status(-99), -2)]);
        let object = load_object();
        // SAFETY: this crate's plugin object.
        unsafe { call_all_plugins_loaded(object) };
        let answer = metamod::plugins_answer().expect("the table answers");
        assert!(answer.contains(r#""status":"unknown(-99)""#), "{answer}");
    });
}

/// `OnPluginPause`/`OnPluginUnpause` rebuild the table too (Metamod passes the id:
/// `ITER_PLEVENT(OnPluginPause, pl->m_Id)`, `core/metamod_plugins.cpp:303-305`). A pause on
/// ANOTHER plugin flips that row's status in the snapshot immediately, not one event later.
/// Our OWN pause cannot be seen - Metamod's event iteration skips the acting plugin
/// (`:95-104`: `if (_Xpl->m_Id == plid) continue;`) and sets `Pl_Paused` only after our
/// `Pause` returns (`:713`) - documented in `ops::metamod`'s module docs.
#[test]
fn pause_and_unpause_rebuild_the_table() {
    scratch_log();
    with_plugin(|| {
        MOCK_PLMGR.set_plugins(&[
            row(3, "mm_other.dll", Pl_Status::Pl_Running, -2),
            row(MOCK_PLUGIN_ID, "self-plugin.dll", Pl_Status::Pl_Running, -2),
        ]);
        let object = load_object();
        // SAFETY: this crate's plugin object.
        unsafe { call_all_plugins_loaded(object) };
        let before =
            metamod::current_snapshot().expect("AllPluginsLoaded published").generation;

        // `meta pause 3`: the status change is already in the manager, the event carries
        // the id, and the row flips in the snapshot on the spot.
        MOCK_PLMGR.set_plugins(&[
            row(3, "mm_other.dll", Pl_Status::Pl_Paused, -2),
            row(MOCK_PLUGIN_ID, "self-plugin.dll", Pl_Status::Pl_Running, -2),
        ]);
        // SAFETY: this crate's static listener.
        unsafe { call_pause_event(3, false) };
        let snap = metamod::current_snapshot().expect("OnPluginPause rebuilt");
        assert_eq!(snap.rows[0].status, "paused", "the paused row, on the event");
        assert_eq!(snap.generation, before + 1);

        // ... and `meta unpause 3` back.
        MOCK_PLMGR.set_plugins(&[
            row(3, "mm_other.dll", Pl_Status::Pl_Running, -2),
            row(MOCK_PLUGIN_ID, "self-plugin.dll", Pl_Status::Pl_Running, -2),
        ]);
        // SAFETY: as above.
        unsafe { call_pause_event(3, true) };
        let snap = metamod::current_snapshot().expect("OnPluginUnpause rebuilt");
        assert_eq!(snap.rows[0].status, "running");
        assert_eq!(snap.generation, before + 2);
    });
}

/// A `Query` TRUE whose status is `Pl_NotFound` is a ROW, not the gap: Metamod keeps a
/// `meta load typo` entry in m_Plugins with `Pl_NotFound`
/// (`core/metamod_plugins.cpp:476-483`) and `meta list` shows it as NOFILE
/// (`GetStatusText`, `:426-428`). Only a `Query` FALSE ("not found",
/// `core/IPluginManager.h:~125`) is a hole in the table. (The mock models the real
/// `Query`'s shape: true for every id it holds a row for, whatever the status,
/// `metamod_plugins.cpp:768-793`.)
#[test]
fn a_query_true_notfound_row_is_reported() {
    scratch_log();
    with_plugin(|| {
        MOCK_PLMGR.set_plugins(&[
            // The typo load, kept in the table: the file never opened.
            row(2, "mm_typo.dll", Pl_Status::Pl_NotFound, -2),
            row(MOCK_PLUGIN_ID, "self-plugin.dll", Pl_Status::Pl_Running, -2),
        ]);
        let object = load_object();
        // SAFETY: this crate's plugin object.
        unsafe { call_all_plugins_loaded(object) };
        let snap = metamod::current_snapshot().expect("published");
        assert_eq!(
            snap.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, MOCK_PLUGIN_ID],
            "the typo row is IN the table"
        );
        let typo = &snap.rows[0];
        assert_eq!(
            (typo.file.as_str(), typo.status.as_str(), typo.source),
            ("mm_typo.dll", "not_found", -2)
        );
        assert!(!typo.is_self);
        let answer = metamod::plugins_answer().expect("the table answers");
        assert!(answer.contains(r#""status":"not_found""#), "{answer}");
    });
}

/// `MetaFactory` answers null (a Metamod without the manager): `Load` still SUCCEEDS, and the
/// method answers `-32017` with `blockedOn: "metamod: IPluginManager unavailable"`.
#[test]
fn metafactory_null_answers_blocked_on() {
    scratch_log();
    with_plugin(|| {
        MOCK_PLMGR.refuse_metafactory.store(true, Ordering::SeqCst);
        load_object(); // Load succeeds despite the refusal
        assert!(plugin::is_present());
        assert!(metamod::current_snapshot().is_none(), "no rebuild without a manager");
        let (code, message, data) =
            metamod::plugins_answer().expect_err("no manager, no table");
        assert_eq!(code, -32017);
        assert_eq!(message, "PrerequisiteFailed");
        assert_eq!(data, "{\"blockedOn\":\"metamod: IPluginManager unavailable\"}");
    });
}

/// Metamod absent: `-32017` with `blockedOn: "metamod: not loaded"`.
#[test]
fn metamod_absent_answers_blocked_on() {
    scratch_log();
    with_plugin(|| {
        assert!(!plugin::is_present());
        let (code, message, data) =
            metamod::plugins_answer().expect_err("no Metamod, no table");
        assert_eq!(code, -32017);
        assert_eq!(message, "PrerequisiteFailed");
        assert_eq!(data, "{\"blockedOn\":\"metamod: not loaded\"}");
    });
}

/// The NEGATIVE: the lane's REAL answer path ([`wiring::BridgeState::route`], what
/// `ipc::listener` calls on the IPC thread) never calls into the mock manager - from a
/// SPAWNED thread standing in for the IPC one. The snapshot under the mutex is the whole
/// answer: every `Query` the mock ever saw happened inside a main-thread callback
/// (`Load`/`AllPluginsLoaded`/the listener events), on the thread that drove them.
#[test]
fn the_method_never_calls_the_manager_off_the_callback_thread() {
    scratch_log();
    with_plugin(|| {
        MOCK_PLMGR.set_plugins(&[row(
            MOCK_PLUGIN_ID,
            "mm_our_row.dll",
            Pl_Status::Pl_Running,
            -2,
        )]);
        let object = load_object();
        // SAFETY: this crate's plugin object.
        unsafe { call_all_plugins_loaded(object) };
        let driver = std::thread::current().id();
        assert!(
            !MOCK_PLMGR.calls().is_empty(),
            "the setup drove the callbacks on this thread; the mock must have seen them"
        );
        for call in MOCK_PLMGR.calls() {
            assert_eq!(call.thread, driver, "every Query came from the callback driver");
        }

        struct FakeModules;
        impl ModuleViews for FakeModules {
            fn state(&self, _module: u8) -> registry::ModState {
                registry::ModState::Verified
            }
            fn is_verified(&self, _module: u8) -> bool {
                true
            }
            fn revision(&self) -> u64 {
                1
            }
        }
        let bridge = std::sync::Arc::new(wiring::BridgeState::new(
            std::sync::Arc::new(FakeModules),
            Box::new(dispatch::is_ported),
        ));

        // The lane thread answers `metamod.plugins` the way the listener does - and the
        // mock records whatever manager call THAT thread made. (The bridge is Send: its
        // module source and `ported` predicate are `Send + Sync` by type.)
        MOCK_PLMGR.clear_calls();
        let answer = std::thread::spawn(move || {
            let answer = bridge.route("metamod.plugins");
            (std::thread::current().id(), answer, MOCK_PLMGR.calls())
        })
        .join()
        .expect("the lane thread does not panic");
        let (lane_thread, answer, lane_calls) = answer;
        for call in &lane_calls {
            assert_ne!(call.thread, lane_thread, "the IPC lane called into the manager");
        }
        assert!(
            lane_calls.is_empty(),
            "the lane must read the snapshot, never the manager: {lane_calls:?}"
        );
        match answer {
            Route::Reply(cs2_bridge::ipc::ControlReply::Result(result)) => {
                assert!(result.contains(r#""self":true"#), "{result}");
            }
            other => panic!("the lane must answer the row itself: {other:?}"),
        }
    });
}

/// The row's registration: found by `registry::find` (in `RUST_METHODS`, not the frozen C
/// table), `needs == 0`, served by `is_ported`, advertised in this build's capabilities, and
/// answered by the lane (`BridgeState::route`) - never queued.
#[test]
fn the_row_is_registered_and_answered_on_the_lane() {
    scratch_log();
    with_plugin(|| {
        let found = registry::find("metamod.plugins").expect("the row exists");
        assert_eq!((found.needs, found.implemented, found.hidden), (0, true, false));
        assert_eq!(found.name, metamod::METHOD);
        assert!(dispatch::is_ported("metamod.plugins"));
        assert_eq!(registry::gate("metamod.plugins", 0), registry::Gate::Run);
        // The frozen C table stayed the frozen C table.
        assert!(registry::METHODS.iter().all(|m| m.name != "metamod.plugins"));
        // The row is one of this build's own. (No length pin: a sibling work package may
        // add further Rust rows, and this test does not care.)
        assert!(registry::RUST_METHODS.iter().any(|m| m.name == metamod::METHOD));

        struct FakeModules;
        impl ModuleViews for FakeModules {
            fn state(&self, _module: u8) -> registry::ModState {
                registry::ModState::Verified
            }
            fn is_verified(&self, _module: u8) -> bool {
                true
            }
            fn revision(&self) -> u64 {
                1
            }
        }
        let bridge = wiring::BridgeState::new(
            std::sync::Arc::new(FakeModules),
            Box::new(dispatch::is_ported),
        );

        // Metamod absent: the lane answers the row's -32017 itself, immediately.
        assert!(!plugin::is_present());
        match bridge.route("metamod.plugins") {
            Route::Reply(cs2_bridge::ipc::ControlReply::Error { code, ref message, ref data }) => {
                assert_eq!(code, -32017);
                assert_eq!(message, "PrerequisiteFailed");
                assert_eq!(data.as_deref(), Some("{\"blockedOn\":\"metamod: not loaded\"}"));
            }
            other => panic!("the lane must answer the row itself: {other:?}"),
        }

        // Metamod present: the lane answers from the snapshot - still a Reply, never a Queue
        // (a queued row would need the tick's drain).
        MOCK_PLMGR.set_plugins(&[
            row(1, "mm_plugin_one.dll", Pl_Status::Pl_Running, -1),
            row(MOCK_PLUGIN_ID, "self-plugin.dll", Pl_Status::Pl_Running, -2),
        ]);
        let object = load_object();
        // SAFETY: this crate's plugin object.
        unsafe { call_all_plugins_loaded(object) };
        match bridge.route("metamod.plugins") {
            Route::Reply(cs2_bridge::ipc::ControlReply::Result(result)) => {
                assert!(result.starts_with("{\"plugins\":[{"), "{result}");
                assert!(result.contains(r#""self":true"#), "our id is flagged: {result}");
            }
            other => panic!("the lane must answer the row itself: {other:?}"),
        }

        // And the advertisement: this build's capabilities name the row.
        let caps = bridge.capabilities();
        assert!(caps.contains("\"metamod.plugins\""), "{caps}");
    });
}

/// The method is a lane row end to end: unknown names stay unknown, and the frozen table's
/// differential facts were not disturbed (the in-file tests of `registry`/`dispatch` pin
/// theirs; this pins the boundary between the two tables).
#[test]
fn the_rust_row_does_not_disturb_the_c_table() {
    scratch_log();
    with_plugin(|| {
        // `find` falls through to RUST_METHODS only after the C table.
        assert_eq!(registry::find("control.ping").map(|m| m.name), Some("control.ping"));
        assert!(registry::find("no.such.method").is_none());
        // `decide` treats the row as served and fast-path (needs == 0), like `control.*`.
        assert_eq!(
            dispatch::decide("metamod.plugins", 0, &dispatch::is_ported),
            dispatch::Decision::FastPath
        );
    });
}

/// Point the bridge log at `target/scratch/` for this test binary, and answer the path.
///
/// An integration test links the library **without** `cfg(test)`, so `log::line`'s default
/// path would be the user's real `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log` (`src/log.rs`). A
/// `cargo test` must never append to that file, so every case that can log points
/// `CS2_BRIDGE_LOG` here first. Process-wide and idempotent: the first call sets the variable
/// and every later call answers the same path (a case that logs to its own file restores it
/// with this).
fn scratch_log() -> std::path::PathBuf {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&dir).expect("target/scratch is writable");
        let path = dir.join("cs2-bridge-metamod-plugin-test.log");
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        path
    })
    .clone()
}
