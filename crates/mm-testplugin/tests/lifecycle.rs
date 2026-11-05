//! The fixture's Metamod lifecycle, driven through the generated vtables with a mock
//! `ISmmAPI`, a mock `IKHook` and a fake hooked object.
//!
//! Same discipline as the bridge's `crates/cs2-bridge/tests/metamod_plugin.rs`: every call
//! goes through the table entry Metamod would call (the generated `_vtable` field or a
//! generated `_slots` index, never a hand-written number), the mocks are real objects of the
//! generated types whose tables hold this file's functions, and a mock checks its caller's
//! contract by *recording* a violation (an `assert!` would unwind across the C ABI) that the
//! test body fails on. The mock here stands in for KHook's `IKHook` behind the
//! `s2sdk::khook_vtable` adapter - the `KHookApi` surface the plugin drives through
//! `s2sdk::hook` - so the adapter move (commit 1) is part of what is tested.
//!
//! The mock's per-call value model is the minimal faithful slice of `detour.cpp`:
//!
//! * `SaveReturnValue(action, ptr, size, ..., original)` stores the value it was handed when
//!   the action is `Override`/`Supersede` and `original` is false - the loop's override
//!   storage (`detour.cpp:366-404`), folded the way KHook folds: a higher action replaces a
//!   lower one, and for **equal** actions the first saved value wins (`:381-383`), so a
//!   later save of the same action does not replace the stored one.
//! * `GetOriginalValuePtr` answers the original's byte, `GetOverrideValuePtr` the stored
//!   override, and `GetCurrentValuePtr` the override when one was stored, the original's
//!   otherwise - the rule `KHook::GetCurrentValuePtr` implements (`detour.cpp:453-464`).
//!
//! The API slot of `s2sdk::hook` is process-wide, so the cases serialize on [`TEST_LOCK`],
//! and every case installs its own target resolver before loading.

use core::ffi::{c_char, c_int, c_void};
use core::mem::transmute;
use core::ptr;
use std::sync::{Mutex, MutexGuard};

use mm_testplugin::plugin;
use s2sdk::hook::{self, Action, HookId};
use s2sdk_sys::generated::{
    ISmmAPI, ISmmAPI_vtable, ISmmPlugin, ISmmPlugin_vtable, IKHook, IKHook_vtable,
    ISource2ServerConfig_slots, METAMOD_PLAPI_VERSION,
};

// -----------------------------------------------------------------------------------------
// Recording mock-contract violations
// -----------------------------------------------------------------------------------------

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

/// Take everything recorded since the last call.
fn take_violations() -> Vec<String> {
    std::mem::take(
        &mut *VIOLATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Fail the current test unless no violation is outstanding (and say which).
fn assert_no_violations() {
    let violations = take_violations();
    assert!(violations.is_empty(), "mock contract violations: {violations:?}");
}

fn lock_of<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// -----------------------------------------------------------------------------------------
// The mock IKHook
// -----------------------------------------------------------------------------------------

/// What the mock's `SetupVirtualHook` saw, in order.
#[derive(Clone)]
struct Setup {
    id: HookId,
    vtable: usize,
    index: i32,
    context: usize,
    removed: usize,
    pre: usize,
    post: usize,
    r#async: bool,
}

/// What the mock's `RemoveHook` saw, in order.
#[derive(Clone)]
struct Removal {
    id: HookId,
    r#async: bool,
}

/// What the mock's `SaveReturnValue` saw, in order.
#[derive(Clone)]
struct Saved {
    action: u8,
    value: Option<u8>,
    original: bool,
}

/// The mock `IKHook`'s mutable state. The static's fields are individual mutexes (a shared
/// static cannot hand out `&mut`), reset field by field in [`MOCK_KHOOK.reset`].
struct MockKhook {
    setups: Mutex<Vec<Setup>>,
    removals: Mutex<Vec<Removal>>,
    saves: Mutex<Vec<Saved>>,
    /// The `(id, context, removed_function)` of each successful registration, as KHook
    /// stores in the hook's own details.
    hooks: Mutex<Vec<(HookId, usize, usize)>>,
    /// The ids KHook still holds (`globals().associated_hooks`); `RemoveHook` takes one off
    /// (synchronously, having run its removal function - `detour.cpp:1581-1597`).
    associated: Mutex<Vec<HookId>>,
    /// The context the running call sees (`GetContextPtr`): the one the mock pushes, exactly
    /// as KHook's trampoline pushes the hook's context before entering a thunk
    /// (`detour.cpp:755`).
    current_context: Mutex<usize>,
    /// The original's value byte, seeded by the test: the value the original call produced
    /// (`detour.cpp:895-908` in a call where the original ran).
    original: Mutex<Option<u8>>,
    /// The stored override (`SaveReturnValue` of an `Override`/`Supersede` with
    /// `original = false`, `detour.cpp:366-404`).
    override_value: Mutex<Option<u8>>,
    /// The action the stored override was saved for: the fold's other input
    /// (`detour.cpp:366-404`; equal actions keep the first saved value, `:381-383`).
    override_action: Mutex<Option<u8>>,
}

impl MockKhook {
    /// Reset the mock's records, field by field (a static cannot be replaced wholesale).
    fn reset(&self) {
        lock_of(&self.setups).clear();
        lock_of(&self.removals).clear();
        lock_of(&self.saves).clear();
        lock_of(&self.hooks).clear();
        lock_of(&self.associated).clear();
        *lock_of(&self.current_context) = 0;
        *lock_of(&self.original) = None;
        *lock_of(&self.override_value) = None;
        *lock_of(&self.override_action) = None;
    }
}

/// KHook's hook-id counter: one global of the library (`detour.cpp:1546-1548`, `++` at
/// `:1728-1729` under `hook_id_mutex`), not a counter per `IKHook`. It is process-wide here
/// for the same reason it is in KHook - and because the wrapper's `lost`, `removal_fired`
/// and `removed_before_registration` sets never shrink: an id must never be handed out twice
/// in this binary, or a later case's removal would match an earlier case's record (a repeated
/// id reads as "the removal function already fired", is reconciled as lost, and the drain
/// re-issues it for ever). Same shape as lane A's mock (`s2sdk`'s `tests/hook_mock.rs`).
static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

static MOCK_KHOOK: MockKhook = MockKhook {
    setups: Mutex::new(Vec::new()),
    removals: Mutex::new(Vec::new()),
    saves: Mutex::new(Vec::new()),
    hooks: Mutex::new(Vec::new()),
    associated: Mutex::new(Vec::new()),
    current_context: Mutex::new(0),
    original: Mutex::new(None),
    override_value: Mutex::new(None),
    override_action: Mutex::new(None),
};

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

/// `IKHook::SetupHook` (`khook.hpp:2186`): the fixture registers no inline hook, so reaching
/// it is a failure of the code under test.
unsafe extern "C" fn mock_setup_hook(
    _this: *mut IKHook,
    _function: *mut c_void,
    _context: *mut c_void,
    _removed_function: *mut c_void,
    _pre: *mut c_void,
    _post: *mut c_void,
    _make_return: *mut c_void,
    _make_call_original: *mut c_void,
    _stack_size: u32,
    _is_async: bool,
) -> u32 {
    record_violation("IKHook::SetupHook: this plugin registers no inline hook".to_owned());
    hook::INVALID_HOOK
}

/// `IKHook::SetupVirtualHook` (`khook.hpp:2187`): record the registration and hand out the
/// next id, as `__Setup__VirtualHook` does (`detour.cpp:1786-1810`).
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn mock_setup_virtual_hook(
    this: *mut IKHook,
    vtable: *mut *mut c_void,
    index: i32,
    context: *mut c_void,
    removed_function: *mut c_void,
    pre: *mut c_void,
    post: *mut c_void,
    _make_return: *mut c_void,
    _make_call_original: *mut c_void,
    stack_size: u32,
    r#async: bool,
) -> u32 {
    check(this as usize == expected_khook(), || {
        format!("IKHook::SetupVirtualHook on {this:p}, not the IKHook GetDetourInterface returned")
    });
    // The wrapper's own contract (`s2sdk::hook::register_virtual`): a live vtable, a live
    // context, all four thunks, a non-zero copy stack.
    check(!vtable.is_null(), || "SetupVirtualHook without a vtable".to_owned());
    check(!context.is_null(), || "SetupVirtualHook without a context".to_owned());
    check(!removed_function.is_null(), || {
        "SetupVirtualHook without a removal function".to_owned()
    });
    check(!pre.is_null() && !post.is_null(), || {
        "SetupVirtualHook without callbacks".to_owned()
    });
    check(stack_size > 0, || "SetupVirtualHook with a zero copy stack".to_owned());
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    lock_of(&MOCK_KHOOK.setups).push(Setup {
        id,
        vtable: vtable as usize,
        index,
        context: context as usize,
        removed: removed_function as usize,
        pre: pre as usize,
        post: post as usize,
        r#async,
    });
    lock_of(&MOCK_KHOOK.hooks).push((id, context as usize, removed_function as usize));
    lock_of(&MOCK_KHOOK.associated).push(id);
    id
}

/// `IKHook::RemoveHook` (`khook.hpp:2188`). A synchronous removal runs the hook's removal
/// function before it returns (`detour.cpp:1581-1597`); the fixture only ever asks for
/// synchronous removals (`remove_sync`, and the drain's re-issue).
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
    // The wrapper passes null for both caller-supplied slots (`khook.hpp:2188`'s defaults;
    // see `s2sdk::hook::Hook::remove_sync`).
    check(hook_removal_fn.is_null() && context.is_null(), || {
        format!("RemoveHook with a caller-supplied removal callback: id {id}")
    });
    lock_of(&MOCK_KHOOK.removals).push(Removal { id, r#async });
    if r#async {
        // Never reached today: `remove_sync` and the drain both ask synchronously. A queued
        // removal would need a delete thread to run it, and no test drives that path.
        record_violation(format!("RemoveHook(id={id}, async=true): the fixture never queues"));
        return;
    }
    // The removal function KHook stored at setup, run with the context pushed
    // (`detour.cpp:1531-1541`). A removal of an id KHook does not hold is the header's
    // "not associated" no-op (`detour.cpp:1583-1586`) - that is exactly what the drain's
    // barrier removal (`INVALID_HOOK`, see `remove_all_blocking`) is meant to hit - so no
    // violation: the `removals` record is what the tests read.
    let entry = lock_of(&MOCK_KHOOK.hooks)
        .iter()
        .find(|(held_id, _, _)| *held_id == id)
        .copied();
    if r#async || entry.is_none() {
        // (The async case above already recorded its violation and returned.)
        return;
    }
    let (_, ctx, removed) = entry.unwrap();
    lock_of(&MOCK_KHOOK.associated).retain(|&held| held != id);
    // SAFETY: the wrapper registered this thunk with exactly this signature
    // (`khook.hpp:711-719`), and the mock is standing in for the KHook that calls it.
    let removed: unsafe extern "C" fn(HookId) = unsafe { transmute(removed) };
    *lock_of(&MOCK_KHOOK.current_context) = ctx;
    // SAFETY: as above.
    unsafe { removed(id) };
    *lock_of(&MOCK_KHOOK.current_context) = 0;
}

/// `IKHook::GetContextPtr` (`khook.hpp:2189`): the context pushed for the running call
/// (`detour.cpp:359-361`).
unsafe extern "C" fn mock_get_context_ptr(_this: *mut IKHook) -> *mut c_void {
    *lock_of(&MOCK_KHOOK.current_context) as *mut c_void
}

/// `IKHook::GetOriginalFunction` (`khook.hpp:2190`): not reached by this fixture.
unsafe extern "C" fn mock_get_original_function(_this: *mut IKHook) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::GetOriginalValuePtr` (`khook.hpp:2191`): the original's byte (`detour.cpp:895-908`
/// in a call where the original ran).
unsafe extern "C" fn mock_get_original_value_ptr(_this: *mut IKHook) -> *mut c_void {
    let value = lock_of(&MOCK_KHOOK.original).unwrap_or(0);
    ORIGINAL_BYTE.store(value, std::sync::atomic::Ordering::SeqCst);
    ORIGINAL_BYTE.as_ptr() as *mut c_void
}

/// `IKHook::GetOverrideValuePtr` (`khook.hpp:2192`): the stored override, or null when none.
unsafe extern "C" fn mock_get_override_value_ptr(_this: *mut IKHook) -> *mut c_void {
    match *lock_of(&MOCK_KHOOK.override_value) {
        Some(value) => {
            OVERRIDE_BYTE.store(value, std::sync::atomic::Ordering::SeqCst);
            OVERRIDE_BYTE.as_ptr() as *mut c_void
        }
        None => ptr::null_mut(),
    }
}

/// `IKHook::GetCurrentValuePtr` (`khook.hpp:2193`): the override when one was stored, the
/// original's otherwise (`detour.cpp:453-464`).
unsafe extern "C" fn mock_get_current_value_ptr(_this: *mut IKHook, _pop: bool) -> *mut c_void {
    match *lock_of(&MOCK_KHOOK.override_value) {
        Some(value) => {
            OVERRIDE_BYTE.store(value, std::sync::atomic::Ordering::SeqCst);
            OVERRIDE_BYTE.as_ptr() as *mut c_void
        }
        None => {
            let value = lock_of(&MOCK_KHOOK.original).unwrap_or(0);
            ORIGINAL_BYTE.store(value, std::sync::atomic::Ordering::SeqCst);
            ORIGINAL_BYTE.as_ptr() as *mut c_void
        }
    }
}

/// The one-byte storages the value getters hand out; `static` so the address outlives the
/// call that read it (`detour.cpp`'s per-call storage is alive for the call, and so is this).
/// The cases serialize on [`TEST_LOCK`], so no concurrent getter can swap the byte under a
/// reader.
static ORIGINAL_BYTE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static OVERRIDE_BYTE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// `IKHook::DestroyReturnValue` (`khook.hpp:2194`): the per-call storage is this file's
/// statics; nothing to free.
unsafe extern "C" fn mock_destroy_return_value(_this: *mut IKHook) {}

/// `IKHook::FindOriginal` (`khook.hpp:2195`): not reached by this fixture.
unsafe extern "C" fn mock_find_original(_this: *mut IKHook, _function: *mut c_void) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::FindOriginalVirtual` (`khook.hpp:2196`): not reached by this fixture.
unsafe extern "C" fn mock_find_original_virtual(
    _this: *mut IKHook,
    _vtable: *mut *mut c_void,
    _index: i32,
) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::DoRecall` (`khook.hpp:2197`): not reached by this fixture.
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

/// `IKHook::SaveReturnValue` (`khook.hpp:2198`): record it, and store the override byte when
/// the action is a value change (`detour.cpp:366-404`).
unsafe extern "C" fn mock_save_return_value(
    this: *mut IKHook,
    action: u8,
    ptr_to_return: *mut c_void,
    return_size: usize,
    _init_op: *mut c_void,
    _deinit_op: *mut c_void,
    original: bool,
) {
    check(this as usize == expected_khook(), || {
        format!("IKHook::SaveReturnValue on {this:p}, not the mock's IKHook")
    });
    // The fixture's hook returns `u8`: a saved value is one readable byte.
    check(return_size == 0 || return_size == 1, || {
        format!("SaveReturnValue with size {return_size} for a u8 hook")
    });
    let value = if return_size == 0 || ptr_to_return.is_null() {
        None
    } else {
        // SAFETY: the thunk handed a pointer to the value it is saving (`khook.hpp:293`),
        // readable for the `return_size` it named - one byte here.
        Some(unsafe { *(ptr_to_return as *const u8) })
    };
    if !original && value.is_some() && action >= Action::Override as u8 {
        // KHook's fold (`detour.cpp:366-404`): a higher action replaces a lower one;
        // for equal actions the first saved value wins (`:381-383`) - the stored one
        // stays, a later save of the same action does not replace it.
        let stored_action = *lock_of(&MOCK_KHOOK.override_action);
        if stored_action.is_none_or(|old| action > old) {
            *lock_of(&MOCK_KHOOK.override_value) = value;
            *lock_of(&MOCK_KHOOK.override_action) = Some(action);
        }
    }
    lock_of(&MOCK_KHOOK.saves).push(Saved {
        action,
        value,
        original,
    });
}

/// `IKHook::LookupSignature` (`khook.hpp:2199`): not reached by this fixture.
unsafe extern "C" fn mock_lookup_signature(
    _this: *mut IKHook,
    _start: *mut c_void,
    _size: usize,
    _signature: *const c_char,
) -> *mut c_void {
    ptr::null_mut()
}

/// `IKHook::WasOriginalFunctionSkipped` (`khook.hpp:2200`): not reached by this fixture (the
/// plugin's callbacks never ask).
unsafe extern "C" fn mock_was_original_function_skipped(_this: *mut IKHook) -> bool {
    false
}

// -----------------------------------------------------------------------------------------
// The mock ISmmAPI
// -----------------------------------------------------------------------------------------

/// Whether the mock's `GetDetourInterface` answers the mock `IKHook` (the default) or null
/// (the no-KHook negative case).
static NULL_DETOUR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The mock `ISmmAPI`: only the one slot this fixture uses is filled, every other one stays
/// null like `ISmmAPI_vtable::NULL` (the plugin must not call them; a null slot would fault
/// and the test would fail).
struct SmmVtable(ISmmAPI_vtable);

// SAFETY: the table holds only function addresses and nulls; nothing writes it.
unsafe impl Sync for SmmVtable {}

static MOCK_SMM_VTABLE: SmmVtable = SmmVtable(ISmmAPI_vtable {
    GetDetourInterface: Some(mock_get_detour_interface),
    ..ISmmAPI_vtable::NULL
});

/// The mock `ISmmAPI` object (one vtable pointer).
struct SmmObj(ISmmAPI);

// SAFETY: the object's only field is a pointer to the immutable `MOCK_SMM_VTABLE` static.
unsafe impl Sync for SmmObj {}

static MOCK_SMM_OBJ: SmmObj = SmmObj(ISmmAPI {
    vtable: ptr::addr_of!(MOCK_SMM_VTABLE.0).cast::<c_void>(),
});

/// `ISmmAPI::GetDetourInterface` (`core/ISmmAPI.h:449`): `void* GetDetourInterface(PluginId
/// id)` - Metamod's `KHook::IKHook*` for that plugin (`core/metamod.cpp:1081`).
unsafe extern "C" fn mock_get_detour_interface(_this: *mut ISmmAPI, _id: c_int) -> *mut c_void {
    if NULL_DETOUR.load(std::sync::atomic::Ordering::SeqCst) {
        ptr::null_mut()
    } else {
        ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut().cast::<c_void>()
    }
}

// -----------------------------------------------------------------------------------------
// The fake ISource2ServerConfig
// -----------------------------------------------------------------------------------------

/// One vtable entry of the fake config: `void *`, like the generated arrays.
type VtEntryFake = *const c_void;

/// The fake config object's vtable: the generated slot count, with a real `extern "C"`
/// function at the generated `AllowSaveRestore` slot - what `Load` reads out of the object
/// and hands the hook, and what the (unhooked) entry the trampoline would call looks like.
/// No other entry is ever reached: the fixture hooks exactly this one slot.
struct FakeVtable([VtEntryFake; ISource2ServerConfig_slots::VTLEN]);

// SAFETY: the table is a `const`-initialised static of function addresses and nulls; nothing
// writes it.
unsafe impl Sync for FakeVtable {}

static FAKE_CONFIG_VTABLE: FakeVtable = FakeVtable({
    let mut vt = [ptr::null(); ISource2ServerConfig_slots::VTLEN];
    vt[ISource2ServerConfig_slots::AllowSaveRestore] = fake_allow_save_restore as VtEntryFake;
    vt
});

/// The fake's `AllowSaveRestore` body: answers 1, the way the game's own mostly does. The
/// fixture's `pre` never changes it in `passive` mode; in the supersede modes the final value
/// is what the mock's value model says. This function exists so the hooked slot points at
/// live code.
unsafe extern "C" fn fake_allow_save_restore(_this: *mut c_void) -> u8 {
    1
}

/// The fake `ISource2ServerConfig` object: one word, the vptr - the only field of the
/// generated struct the code under test reads (`Load`'s single dereference, exactly the
/// dereference a real object gets).
struct ConfigObj {
    // Read through the object's raw address (`Load`'s dereference), so the field is dead to
    // the compiler's eye.
    #[allow(dead_code)]
    vptr: *const c_void,
}

// SAFETY: the object's only field is a pointer to the immutable `FAKE_CONFIG_VTABLE` static.
unsafe impl Sync for ConfigObj {}

static CONFIG_OBJ: ConfigObj = ConfigObj {
    vptr: ptr::addr_of!(FAKE_CONFIG_VTABLE).cast::<c_void>(),
};

/// The fake config object's address, as the hooked call's `this`.
fn config_obj_addr() -> *mut c_void {
    ptr::addr_of!(CONFIG_OBJ).cast_mut().cast::<c_void>()
}

/// The test resolver: the fake object, exactly what `resolve_live_server` would produce for a
/// real one.
fn fake_target() -> Option<plugin::HookTarget> {
    Some(plugin::HookTarget {
        object: config_obj_addr(),
        slot: ISource2ServerConfig_slots::AllowSaveRestore as i32,
    })
}

/// The resolver for the no-target negative case.
fn no_target() -> Option<plugin::HookTarget> {
    None
}

// -----------------------------------------------------------------------------------------
// Calling the plugin through its own tables
// -----------------------------------------------------------------------------------------

/// The plugin object's address (`CreateInterface("ISmmPlugin")`'s answer), through the real
/// export.
///
/// `CreateInterface` writes one log line (`lib.rs`), and a log line must never reach the
/// live default path (review S2, round 3): the cases that load run under [`Reset`], but a
/// bare object query - the `get_` cases below - used not to, and its line landed in
/// `$XDG_RUNTIME_DIR/cs2mcpbridge/mm_testplugin.log`. The pin therefore happens *here*,
/// before the export runs: every caller of [`plugin_object`] holds [`TEST_LOCK`], so this
/// cannot race another case's env writes, and the value is the same file [`Reset`] pins.
fn plugin_object() -> *mut c_void {
    std::env::set_var("CS2MCP_TESTPLUGIN_LOG", test_log_path());
    // SAFETY: the export's contract (`lib.rs`): `name` is a valid NUL-terminated string that
    // outlives the call, and `ret` may be null.
    unsafe { mm_testplugin::CreateInterface(c"ISmmPlugin".as_ptr(), ptr::null_mut()) }
}

/// Call the vtable entry Metamod would call for `Load`
/// (`core/metamod_plugins.cpp:619`, through the generated `Load` slot).
unsafe fn call_load(
    object: *mut c_void,
    id: c_int,
    late: bool,
    error: *mut c_char,
    maxlen: usize,
) -> bool {
    let obj: *const ISmmPlugin = object.cast();
    // SAFETY: the object's vptr is the generated table; the entry's shape is the generated
    // `Load` field's.
    let f = unsafe { (*(*obj).vtable.cast::<ISmmPlugin_vtable>()).Load }
        .expect("the generated Load slot");
    // SAFETY: Metamod's call contract, with the mock standing in for it.
    unsafe {
        f(
            object.cast(),
            id,
            ptr::addr_of!(MOCK_SMM_OBJ.0).cast_mut(),
            error,
            maxlen,
            late,
        )
    }
}

/// Call the vtable entry for `Unload` (`core/metamod_plugins.cpp:656`).
unsafe fn call_unload(object: *mut c_void, error: *mut c_char, maxlen: usize) -> bool {
    let obj: *const ISmmPlugin = object.cast();
    // SAFETY: as in `call_load`.
    let f = unsafe { (*(*obj).vtable.cast::<ISmmPlugin_vtable>()).Unload }
        .expect("the generated Unload slot");
    // SAFETY: Metamod's call contract.
    unsafe { f(object.cast(), error, maxlen) }
}

/// A NUL-terminated string out of Metamod's error buffer.
fn cstr(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .split(|&b| b == 0)
        .next()
        .unwrap_or(&[])
        .iter()
        .map(|&b| b as u8)
        .collect();
    String::from_utf8(bytes).unwrap_or_default()
}

// -----------------------------------------------------------------------------------------
// The harness
// -----------------------------------------------------------------------------------------

/// The API slot of `s2sdk::hook` is process-wide, and so is the resolver and the process
/// environment: one case at a time.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// The suite's own log file, under the worktree's `target/`: never the plugin's default
/// log path, which on a dev host is the live bridge-side log (review S2).
fn test_log_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/mm-testplugin has two ancestors")
        .join("target")
        .join("mm-testplugin-test.log")
}

/// The plugin's default log path for this process's environment, re-derived from the
/// documented rule (`mm_testplugin::log::resolve`'s, which is private to the crate):
/// Windows `Z:\app\mm_testplugin.log`, Linux
/// `$XDG_RUNTIME_DIR/cs2mcpbridge/mm_testplugin.log`, and nothing when the rule has no
/// inputs.
fn default_log_path() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        return Some(std::path::PathBuf::from(r"Z:\app\mm_testplugin.log"));
    }
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(|d| std::path::Path::new(&d).join("cs2mcpbridge").join("mm_testplugin.log"))
}

/// Reset: the resolver, the plugin's state, the mock's records. Runs on `Drop`, so a failing
/// assertion still restores the process for the next case.
struct Reset {
    previous_resolver: fn() -> Option<plugin::HookTarget>,
}

impl Reset {
    fn new(resolver: fn() -> Option<plugin::HookTarget>) -> Self {
        let previous_resolver = plugin::hook_target_resolver();
        plugin::set_hook_target_resolver(resolver);
        // Every log line the plugin writes in a test goes to this suite's own file under
        // `target/`: with the env var unset the plugin's default rule would name the live
        // bridge-side log on this host (`$XDG_RUNTIME_DIR/cs2mcpbridge/...`), and a test
        // run must never write that. The var is left set for the process's remaining
        // lifetime - the log path outliving a case is as harmless as the log itself.
        std::env::set_var("CS2MCP_TESTPLUGIN_LOG", test_log_path());
        Self { previous_resolver }
    }
}

impl Drop for Reset {
    fn drop(&mut self) {
        plugin::set_hook_target_resolver(self.previous_resolver);
        // The env var is process state: the next case decides its own mode.
        set_mode_env(None);
        plugin::reset_evidence();
        // An Unload of a still-loaded plugin (a failed case) - best effort, through the
        // table, ignoring the answer: the state is process-wide and the next case needs it
        // clean.
        if plugin::is_present() {
            let mut error = [0 as c_char; 256];
            // SAFETY: this crate's plugin object.
            let _ = unsafe { call_unload(plugin_object(), error.as_mut_ptr(), error.len()) };
        }
        NULL_DETOUR.store(false, std::sync::atomic::Ordering::SeqCst);
        MOCK_KHOOK.reset();
        take_violations();
    }
}

/// Set the mode env var for this case (and let [`Reset`]'s caller see it back out is not
/// needed: the next case sets its own, and `passive` cases unset it).
fn set_mode_env(mode: Option<&str>) {
    if let Some(mode) = mode {
        std::env::set_var("CS2MCP_TESTPLUGIN_MODE", mode);
    } else {
        std::env::remove_var("CS2MCP_TESTPLUGIN_MODE");
    }
}

/// Load through the table, with the mock Metamod. Fails the test if the load refused.
fn load_ok(id: c_int) -> *mut c_void {
    let object = plugin_object();
    let mut error = [0 as c_char; 256];
    // SAFETY: this crate's plugin object, live mock API.
    let ok = unsafe { call_load(object, id, false, error.as_mut_ptr(), error.len()) };
    assert_eq!(cstr(&error), "", "a successful Load writes no error");
    assert!(ok, "Load must succeed: {}", cstr(&error));
    assert!(plugin::is_present());
    assert_no_violations();
    object
}

// -----------------------------------------------------------------------------------------
// The cases
// -----------------------------------------------------------------------------------------

/// `Load` registers exactly one virtual hook: the object's vptr, the generated slot,
/// `async = false` (`SetupMode::Sync`).
#[test]
fn load_sets_up_exactly_one_virtual_hook_on_the_generated_slot() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    let object = load_ok(3);

    let setups = lock_of(&MOCK_KHOOK.setups).clone();
    assert_eq!(setups.len(), 1, "exactly one SetupVirtualHook");
    let setup = &setups[0];
    assert_eq!(
        setup.vtable,
        ptr::addr_of!(FAKE_CONFIG_VTABLE.0).addr(),
        "the hook goes to the object's own vptr (read through the object)"
    );
    assert_eq!(
        setup.index,
        ISource2ServerConfig_slots::AllowSaveRestore as i32,
        "the hooked slot is the generated one"
    );
    assert!(!setup.r#async, "SetupMode::Sync is the header's async = false");
    assert_ne!(setup.pre, setup.post, "pre and post are distinct thunks");
    assert_ne!(setup.removed, 0, "the removal function is one of the five thunks");
    assert_eq!(plugin::calls_pre(), 0, "no callback has run yet");
    assert_eq!(plugin::calls_post(), 0);
    let _ = object;
}

/// Driving the generated pre/post thunks the mock recorded, per mode: the `pre`'s answer is
/// what the mock's `SaveReturnValue` sees, and the `post`'s final value follows the mode.
///
/// The trampoline's slice being modelled (`detour.cpp:746-990`): the context is pushed, the
/// `pre` thunks run and save, the original runs (seeded to 1 by the case - the game's own
/// answer), the `post` thunks run and read the final value.
fn drive_pre_and_post_per_mode(mode: Option<&str>, expected_action: u8, expected_final: i32) {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    set_mode_env(mode);

    load_ok(3);
    let setup = lock_of(&MOCK_KHOOK.setups).clone().remove(0);
    // SAFETY: the mock recorded the generated pre thunk of `u8 (this)`, whose machine
    // signature is exactly this.
    let pre: unsafe extern "C" fn(*mut c_void) -> u8 = unsafe { transmute(setup.pre) };
    // SAFETY: as above, for the post thunk.
    let post: unsafe extern "C" fn(*mut c_void) -> u8 = unsafe { transmute(setup.post) };

    // The original's value, as the original call would have produced it
    // (`detour.cpp:895-908`): the game's own answer, seeded 1.
    *lock_of(&MOCK_KHOOK.original) = Some(1);
    *lock_of(&MOCK_KHOOK.current_context) = setup.context;

    // SAFETY: the mock stands in for KHook's trampoline (the context is pushed above); the
    // argument is the hooked object's `this`.
    let pre_return = unsafe { pre(config_obj_addr()) };
    assert_eq!(pre_return, 0, "the pre thunk answers the context's fake_return");

    // SAFETY: as above.
    unsafe { post(config_obj_addr()) };

    let saves = lock_of(&MOCK_KHOOK.saves).clone();
    assert_eq!(saves.len(), 2, "one save per thunk: pre, post");
    let pre_save = &saves[0];
    assert_eq!(pre_save.action, expected_action, "the pre's folded action");
    assert!(!pre_save.original, "a pre's save is never the original's");
    // The saved byte is what the fold gave KHook: an `Ignore`'s fake_return (0) or the
    // supersede value (`detour.cpp:366-404` saves it per call).
    let expected_saved = if expected_action == Action::Ignore as u8 {
        0
    } else {
        expected_final as u8
    };
    assert_eq!(pre_save.value, Some(expected_saved), "the saved byte");
    let post_save = &saves[1];
    assert_eq!(post_save.action, Action::Ignore as u8, "the post answers Ignore");

    assert_eq!(plugin::calls_pre(), 1);
    assert_eq!(plugin::calls_post(), 1);
    assert_eq!(plugin::last_final(), expected_final, "the recorded final value");
    assert_no_violations();
}

/// The default (env var unset) is `passive`: `Ignore`, the original's 1.
#[test]
fn passive_is_the_default_mode() {
    drive_pre_and_post_per_mode(None, Action::Ignore as u8, 1);
}

/// The explicit `passive` mode: `Ignore`, the original's 1.
#[test]
fn passive_mode_answers_ignore() {
    drive_pre_and_post_per_mode(Some("passive"), Action::Ignore as u8, 1);
}

/// `supersede0`: `Supersede(0)` - the stored override is the final value.
#[test]
fn supersede0_answers_supersede_zero() {
    drive_pre_and_post_per_mode(Some("supersede0"), Action::Supersede as u8, 0);
}

/// `supersede1`: `Supersede(1)`.
#[test]
fn supersede1_answers_supersede_one() {
    drive_pre_and_post_per_mode(Some("supersede1"), Action::Supersede as u8, 1);
}

/// Two equal-action value saves on one call keep the first saved value - the fold the
/// coexistence expectations lean on: on one `AllowSaveRestore` call, two plugins' `pre`
/// thunks that both answer `Supersede` store through [`SaveReturnValue`]
/// (`khook.hpp:293`), and KHook keeps the first stored one for equal actions
/// (`detour.cpp:381-383`) - only a *higher* action (`Ignore` < `Override` < `Supersede`,
/// `khook.hpp:744-752`) replaces it. The mock's `SaveReturnValue` implements that fold
/// (its value model above), and the case drives the mock's own `IKHook` entries exactly
/// the way the trampoline would: `this` is the mock's object, a `u8` save is one byte.
#[test]
fn equal_action_saves_keep_the_first_value() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    let khook = ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut();

    // Two equal-action saves on one call: the first saved one (0) is what stays, a
    // later save of the same action does not replace it (`detour.cpp:381-383`).
    // SAFETY: the mock's own object and a one-byte save, the call the thunk makes.
    unsafe { save_value(khook, Action::Supersede as u8, 0) };
    // SAFETY: the mock's own object, one byte.
    unsafe { save_value(khook, Action::Supersede as u8, 1) };
    // SAFETY: the mock's own object.
    assert_eq!(unsafe { override_value(khook) }, Some(0), "the first save is stored");
    // SAFETY: the mock's own object.
    assert_eq!(unsafe { current_value(khook) }, 0, "GetCurrentValuePtr answers it");
    // SAFETY: the mock's own object.
    assert_eq!(unsafe { current_value(khook) }, 0, "read again - stable");

    // A *lower* action does not replace it either.
    // SAFETY: the mock's own object, one byte.
    unsafe { save_value(khook, Action::Override as u8, 2) };
    // SAFETY: the mock's own object.
    assert_eq!(unsafe { override_value(khook) }, Some(0), "Override loses to a stored Supersede");

    // On a fresh call's storage (the mock's records reset, as [`Reset`] does between
    // cases), the higher action wins when it comes second.
    MOCK_KHOOK.reset();
    // SAFETY: the mock's own object, one byte.
    unsafe { save_value(khook, Action::Override as u8, 2) };
    // SAFETY: the mock's own object, one byte.
    unsafe { save_value(khook, Action::Supersede as u8, 1) };
    // SAFETY: the mock's own object.
    assert_eq!(unsafe { override_value(khook) }, Some(1), "a higher action replaces");
    assert_no_violations();
}

/// The mock's `SaveReturnValue` entry, driven through the generated vtable (one byte, not
/// the original's - the way a `pre` thunk's save reads).
///
/// # Safety
///
/// `khook` must be the mock's own `IKHook` object ([`MOCK_KHOOK_OBJ`], the `this` the
/// mock's contract check demands); the call otherwise performs no memory effects beyond
/// the mock's records.
unsafe fn save_value(khook: *mut IKHook, action: u8, value: u8) {
    let f = MOCK_KHOOK_VTABLE
        .SaveReturnValue
        .expect("the generated SaveReturnValue slot");
    let mut byte = value;
    // SAFETY: the generated entry's contract, with the mock standing in for KHook; the
    // byte outlives the call (a local is stored through the pointer only inside it).
    unsafe {
        f(
            khook,
            action,
            ptr::addr_of_mut!(byte).cast(),
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            false,
        )
    };
}

/// The mock's `GetOverrideValuePtr` entry: the stored override byte, or `None` when no
/// override was stored on this call.
///
/// # Safety
///
/// `khook` must be the mock's own `IKHook` object; the returned pointer is only read
/// inside this function.
unsafe fn override_value(khook: *mut IKHook) -> Option<u8> {
    let f = MOCK_KHOOK_VTABLE
        .GetOverrideValuePtr
        .expect("the generated GetOverrideValuePtr slot");
    // SAFETY: as in `save_value`.
    let ptr = unsafe { f(khook) };
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the mock's entry hands a pointer to the one-byte `OVERRIDE_BYTE` static.
        Some(unsafe { *(ptr as *const u8) })
    }
}

/// The mock's `GetCurrentValuePtr` entry (`pop = false`): the override's byte when one was
/// stored, the original's otherwise (`detour.cpp:453-464`).
///
/// # Safety
///
/// `khook` must be the mock's own `IKHook` object; the returned pointer is only read
/// inside this function.
unsafe fn current_value(khook: *mut IKHook) -> u8 {
    let f = MOCK_KHOOK_VTABLE
        .GetCurrentValuePtr
        .expect("the generated GetCurrentValuePtr slot");
    // SAFETY: as in `save_value`.
    let ptr = unsafe { f(khook, false) };
    // SAFETY: the mock's entry hands a pointer to a one-byte static (`OVERRIDE_BYTE` or
    // `ORIGINAL_BYTE`).
    unsafe { *(ptr as *const u8) }
}

/// An unknown mode refuses the load with a message, and registers nothing (NEGATIVE).
#[test]
fn unknown_mode_refuses_the_load() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    set_mode_env(Some("supersede2"));

    let object = plugin_object();
    let mut error = [0 as c_char; 256];
    // SAFETY: this crate's plugin object, live mock API.
    let ok = unsafe { call_load(object, 3, false, error.as_mut_ptr(), error.len()) };
    assert!(!ok, "an unknown mode must refuse the Load");
    let message = cstr(&error);
    assert!(
        message.contains("unknown CS2MCP_TESTPLUGIN_MODE 'supersede2'"),
        "{message}"
    );
    assert!(!plugin::is_present());
    assert!(lock_of(&MOCK_KHOOK.setups).is_empty(), "nothing registered");
    // And the adapter was not left installed by the refusal path.
    assert!(hook::api().is_none(), "a refused Load installs nothing");
    assert_no_violations();
}

/// `GetDetourInterface` returning null refuses the load, registers nothing (NEGATIVE): there
/// is no KHook to install, and a plugin in that state must not proceed.
#[test]
fn null_detour_interface_refuses_the_load() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    NULL_DETOUR.store(true, std::sync::atomic::Ordering::SeqCst);

    let object = plugin_object();
    let mut error = [0 as c_char; 256];
    // SAFETY: this crate's plugin object, mock Metamod answering null.
    let ok = unsafe { call_load(object, 3, false, error.as_mut_ptr(), error.len()) };
    assert!(!ok, "a null IKHook must refuse the Load");
    let message = cstr(&error);
    assert!(message.contains("GetDetourInterface returned null"), "{message}");
    assert!(!plugin::is_present());
    assert!(lock_of(&MOCK_KHOOK.setups).is_empty(), "nothing registered");
    assert!(hook::api().is_none(), "a refused Load installs nothing");
    assert_no_violations();
}

/// No target (no server module) refuses the load, registers nothing, and leaves no adapter
/// (NEGATIVE).
#[test]
fn no_server_module_refuses_the_load() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(no_target);

    let object = plugin_object();
    let mut error = [0 as c_char; 256];
    // SAFETY: this crate's plugin object, live mock API, no server module.
    let ok = unsafe { call_load(object, 3, false, error.as_mut_ptr(), error.len()) };
    assert!(!ok, "no ISource2ServerConfig must refuse the Load");
    let message = cstr(&error);
    assert!(message.contains("no ISource2ServerConfig"), "{message}");
    assert!(!plugin::is_present());
    assert!(lock_of(&MOCK_KHOOK.setups).is_empty(), "nothing registered");
    assert!(hook::api().is_none(), "a refused Load installs nothing");
    assert_no_violations();
}

/// `Unload` removes the hook synchronously (exactly one `RemoveHook` with `async = false`),
/// then uninstalls the adapter.
#[test]
fn unload_removes_the_hook_synchronously() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    let object = load_ok(3);

    let mut error = [0 as c_char; 256];
    // SAFETY: this crate's plugin object, whose hook the mock KHook holds.
    let ok = unsafe { call_unload(object, error.as_mut_ptr(), error.len()) };
    assert!(ok, "Unload must succeed: {}", cstr(&error));
    assert!(!plugin::is_present());

    let removals = lock_of(&MOCK_KHOOK.removals).clone();
    let setup_id = lock_of(&MOCK_KHOOK.setups)[0].id;
    assert!(!removals.is_empty(), "at least the hook's own removal");
    assert_eq!(removals[0].id, setup_id, "the id SetupVirtualHook handed out");
    assert!(!removals[0].r#async, "remove_sync asks synchronously");
    // Exactly one removal follows the hook's own, and it is the barrier. `remove_sync`
    // took the id out of the registry's `installed` before asking KHook, so
    // `remove_all_blocking`'s id loop has nothing to re-issue; `lost` is empty in this
    // healthy run, so the drain (one more synchronous removal per lost id, re-issued on
    // every call because the ids are permanent - `s2sdk::hook::remove_all_blocking`,
    // `hook.rs`'s "The drain" block) re-issues nothing either. What is left is the
    // barrier: a removal function of this module ran inside `remove_sync` (setting
    // `Registry::unbarriered_removal`), the count reached zero while it was still on
    // KHook's thread, and one more synchronous removal of `INVALID_HOOK` - an id KHook
    // does not hold, so it runs no removal function of its own - waits for
    // `associated_hooks_mutex` exclusively behind that function's return
    // (`__RemoveHook_Sync`, `detour.cpp:1581-1589`).
    assert_eq!(
        removals.len(),
        2,
        "the hook's own removal plus exactly one barrier removal"
    );
    assert_eq!(removals[1].id, hook::INVALID_HOOK, "the barrier's removal");
    assert!(!removals[1].r#async, "the barrier asks synchronously");
    // The removal function ran (the mock ran it inside the synchronous call,
    // `detour.cpp:1581-1597`), so the hook's context is no longer live.
    assert_eq!(hook::live_context_count(), 0, "the context was freed");
    assert_no_violations();
}

/// The suite never writes the plugin's default log (review S2): every case runs under
/// [`Reset`], which points `CS2MCP_TESTPLUGIN_LOG` at the suite's own file under
/// `target/`, so the default path - the live bridge-side log on a dev host - must be
/// untouched by a full `Load`/call/`Unload` cycle. The suite file, in contrast, must
/// have the cycle's lines.
#[test]
fn the_suite_does_not_touch_the_default_log() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);

    let default_before = default_log_path().and_then(|path| std::fs::read_to_string(path).ok());
    let object = load_ok(3);
    let mut error = [0 as c_char; 256];
    // SAFETY: this crate's plugin object, live mock API.
    let ok = unsafe { call_unload(object, error.as_mut_ptr(), error.len()) };
    assert!(ok, "Unload must succeed: {}", cstr(&error));

    let default_after = default_log_path().and_then(|path| std::fs::read_to_string(path).ok());
    assert_eq!(
        default_before, default_after,
        "the default log path was written by the suite (the Reset must pin \
         CS2MCP_TESTPLUGIN_LOG)"
    );
    let suite_log = std::fs::read_to_string(test_log_path())
        .expect("the suite's log file under target/ was written");
    assert!(
        suite_log.contains("Load(mode=passive)") && suite_log.contains("Unload: removed"),
        "the cycle's lines went to the suite's log:\n{suite_log}"
    );
}

/// The round-3 review escape, as a permanent guard: a bare object query - `CreateInterface`'s
/// one log line, the way the `get_` cases ask - must not create the default log even with
/// `CS2MCP_TESTPLUGIN_LOG` unset. The earlier guard above held only the cases that load;
/// before the round-3 fix, `plugin_object` itself logged to the default path, and running
/// just the `get_` tests created `target/xdg-check/cs2mcpbridge/mm_testplugin.log`
/// (reproduced). The case points `XDG_RUNTIME_DIR` at a scratch directory under `target/`,
/// asks for the object, and asserts the default path never appears - the pin
/// [`plugin_object`] does before the export is what this holds.
#[test]
#[cfg(not(windows))]
fn an_object_query_does_not_create_the_default_log() {
    let _test = lock_of(&TEST_LOCK);

    let xdg = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/mm-testplugin has two ancestors")
        .join("target")
        .join("xdg-check");
    let default_path = xdg.join("cs2mcpbridge").join("mm_testplugin.log");
    let _ = std::fs::remove_dir_all(&xdg);

    // The env is process state: save both variables, name the scratch dir, unset the log
    // override, and restore both afterwards (the next case decides its own).
    let previous_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let previous_log = std::env::var("CS2MCP_TESTPLUGIN_LOG").ok();
    std::env::set_var("XDG_RUNTIME_DIR", &xdg);
    std::env::remove_var("CS2MCP_TESTPLUGIN_LOG");
    let _object = plugin_object();
    let restore =
        |name: &str, previous: Option<String>| match previous {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        };
    restore("XDG_RUNTIME_DIR", previous_xdg);
    restore("CS2MCP_TESTPLUGIN_LOG", previous_log);
    let created = default_path.exists();
    let _ = std::fs::remove_dir_all(&xdg);

    assert!(
        !created,
        "an object query created the default log at {default_path:?}"
    );
}

/// An `Unload` of a never-loaded plugin answers true and touches nothing (Metamod unloads
/// plugins whose `Load` failed too, `core/metamod_plugins.cpp:650`).
#[test]
fn unload_of_a_never_loaded_plugin_touches_nothing() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    let object = plugin_object();

    let mut error = [0 as c_char; 256];
    // SAFETY: this crate's plugin object, never loaded.
    let ok = unsafe { call_unload(object, error.as_mut_ptr(), error.len()) };
    assert!(ok);
    assert!(lock_of(&MOCK_KHOOK.removals).is_empty(), "nothing to remove");
    assert!(hook::api().is_none(), "nothing was ever installed");
    assert_no_violations();
}

/// `GetApiVersion` through the table is the generated 18.
///
/// Even though it never loads, the case takes [`TEST_LOCK`] (its [`plugin_object`] call
/// touches the process env) and runs under [`Reset`] like every other case: "every case
/// runs under Reset" stays a true sentence, and the default-log guard below holds for
/// these queries too.
#[test]
fn get_api_version_is_the_generated_18() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    let object = plugin_object();
    let obj: *const ISmmPlugin = object.cast();
    // SAFETY: the object's vptr is the generated table.
    let f = unsafe { (*(*obj).vtable.cast::<ISmmPlugin_vtable>()).GetApiVersion }
        .expect("the generated GetApiVersion slot");
    // SAFETY: a pure query.
    assert_eq!(unsafe { f(object.cast()) }, METAMOD_PLAPI_VERSION as c_int);
}

/// `GetLogTag` through the table is the fixture's tag.
///
/// [`TEST_LOCK`] + [`Reset`] for the same reason as [`get_api_version_is_the_generated_18`].
#[test]
fn get_log_tag_is_the_fixture_tag() {
    let _test = lock_of(&TEST_LOCK);
    let _reset = Reset::new(fake_target);
    let object = plugin_object();
    let obj: *const ISmmPlugin = object.cast();
    // SAFETY: the object's vptr is the generated table.
    let f = unsafe { (*(*obj).vtable.cast::<ISmmPlugin_vtable>()).GetLogTag }
        .expect("the generated GetLogTag slot");
    // SAFETY: the answer is a static NUL-terminated string.
    let tag = unsafe { core::ffi::CStr::from_ptr(f(object.cast())) };
    assert_eq!(tag.to_bytes(), b"mm-testplugin");
}
