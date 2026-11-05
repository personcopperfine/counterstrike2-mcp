//! The GameFrame tick on a host without the tools framework, driven through the generated tables.
//!
//! Two mocks, in the shape [`tests/metamod_plugin.rs`] uses (the same recording-not-panicking
//! rule: an `assert!` in an `extern "C"` entry would unwind across the ABI):
//!
//! * a mock `IKHook` (`khook.hpp:2186-2200`) that records `SetupVirtualHook`/`RemoveHook` and
//!   models KHook's per-thread context stack (`GetContextPtr`, `detour.cpp:359-361`);
//! * a mock `ISmmAPI` whose `GetServerFactory`/`VInterfaceMatch` answer a fabricated
//!   `ISource2Server` object, plus variants for every refusal in `gameframe::install`.
//!
//! The callback is driven the way KHook's trampoline drives it: with the hook's context pushed,
//! by calling the `pre`/`post` **thunk addresses the mock was given at setup** - not a copy of
//! the callback. A tick is observed through [`cs2_bridge::wiring`]'s own effect on the queue (a
//! pushed request is drained) and through the `gameframe tick N` line in a scratch log.
//!
//! This binary never reaches `toolsys::object()`, so its role set is the Metamod role only -
//! the state the tick exists for. The tool-role case is `tests/gameframe_tool.rs`'s, because
//! `log::Roles` only grows within one process.

use core::cell::RefCell;
use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cs2_bridge::iface;
use cs2_bridge::queue::{self, Request};
use cs2_bridge::gameframe;
use s2sdk::hook;
use s2sdk_sys::generated::{
    IKHook, IKHook_slots, IKHook_vtable, ISmmAPI, ISmmAPI_slots, ISmmAPI_vtable, ISmmPlugin,
    ISmmPlugin_slots, ISource2Server, ISource2Server_slots,
    INTERFACEVERSION_SERVERGAMEDLL,
};

// ---------------------------------------------------------------------------------------
// Recording mock-contract violations (never panic in an extern "C" entry)
// ---------------------------------------------------------------------------------------

static VIOLATIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_violation(message: String) {
    VIOLATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(message);
}

fn check(condition: bool, message: impl FnOnce() -> String) {
    if !condition {
        record_violation(message());
    }
}

fn take_violations() -> Vec<String> {
    std::mem::take(
        &mut *VIOLATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------
// The mock IKHook
// ---------------------------------------------------------------------------------------

/// One `SetupVirtualHook` call (`khook.hpp:2187`).
#[derive(Debug, Clone, Copy)]
struct VirtualSetup {
    vtable: usize,
    index: i32,
    context: usize,
    pre: usize,
    post: usize,
    r#async: bool,
}

/// One `RemoveHook` call (`khook.hpp:2188`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Removal {
    id: u32,
    r#async: bool,
}

static SETUPS: Mutex<Vec<VirtualSetup>> = Mutex::new(Vec::new());
static REMOVALS: Mutex<Vec<Removal>> = Mutex::new(Vec::new());
/// The hooks the mock still holds, as KHook does (`detour.cpp:1531-1541`: the removal runs with
/// the registering context pushed).
static HELD: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
/// Answer `INVALID_HOOK` from the next `SetupVirtualHook` (`khook.hpp:2229`).
static FAIL_SETUP: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// KHook's per-thread context stack (`detour.cpp:359-361`), read by `GetContextPtr`.
    static THREAD_CONTEXT: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn reset_mock() {
    lock(&SETUPS).clear();
    lock(&REMOVALS).clear();
    lock(&HELD).clear();
    FAIL_SETUP.store(false, Ordering::SeqCst);
}

fn setups() -> Vec<VirtualSetup> {
    lock(&SETUPS).clone()
}

fn removals() -> Vec<Removal> {
    lock(&REMOVALS).clone()
}

fn the_setup() -> VirtualSetup {
    let all = setups();
    assert_eq!(all.len(), 1, "one registration per case: {all:?}");
    all[0]
}

/// The hook's removal function, run the way KHook runs it for a synchronous removal: with the
/// registering context pushed (`detour.cpp:1536-1541`).
extern "C" fn run_removal(id: u32, removed: usize, context: usize) {
    THREAD_CONTEXT.with(|stack| stack.borrow_mut().push(context));
    // SAFETY: `removed` is the `extern "C" fn(HookId)` the wrapper registered as this hook's
    // removal function (`khook_thunks!`) and `context` the context it registered with.
    unsafe {
        let f: unsafe extern "C" fn(u32) = core::mem::transmute(removed);
        f(id);
    }
    THREAD_CONTEXT.with(|stack| stack.borrow_mut().pop());
}

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
    _async: bool,
) -> u32 {
    record_violation("SetupHook: the GameFrame tick registers a virtual hook only".to_owned());
    hook::INVALID_HOOK
}

/// `IKHook::SetupVirtualHook` (`khook.hpp:2187`): `HookID_t SetupVirtualHook(void** vtable,
/// int index, void* context, void* removed_function, void* pre, void* post, void* make_return,
/// void* make_call_original, unsigned int stack_size, bool async)`.
unsafe extern "C" fn mock_setup_virtual_hook(
    _this: *mut IKHook,
    vtable: *mut *mut c_void,
    index: i32,
    context: *mut c_void,
    removed_function: *mut c_void,
    _pre: *mut c_void,
    post: *mut c_void,
    _make_return: *mut c_void,
    _make_call_original: *mut c_void,
    stack_size: u32,
    r#async: bool,
) -> u32 {
    check(!vtable.is_null(), || "SetupVirtualHook without a table".to_owned());
    check(!context.is_null(), || "SetupVirtualHook without a context".to_owned());
    check(!removed_function.is_null(), || {
        "SetupVirtualHook without a removal function".to_owned()
    });
    check(!post.is_null(), || "SetupVirtualHook without a post callback".to_owned());
    check(stack_size > 0, || "SetupVirtualHook with a zero copy stack".to_owned());
    lock(&SETUPS).push(VirtualSetup {
        vtable: vtable as usize,
        index,
        context: context as usize,
        pre: _pre as usize,
        post: post as usize,
        r#async,
    });
    if FAIL_SETUP.load(Ordering::SeqCst) {
        // `khook.hpp:2229`: the setup failed, nothing registered.
        return hook::INVALID_HOOK;
    }
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst) as u32;
    lock(&HELD).push((id, removed_function as usize));
    id
}

/// `IKHook::RemoveHook` (`khook.hpp:2188`). An asynchronous removal only queues (the mock's
/// delete thread never runs), which is what makes the teardown's choice observable.
unsafe extern "C" fn mock_remove_hook(
    _this: *mut IKHook,
    id: u32,
    r#async: bool,
    hook_removal_fn: *const c_void,
    context: *mut c_void,
) {
    check(hook_removal_fn.is_null() && context.is_null(), || {
        "RemoveHook with a caller-supplied removal callback".to_owned()
    });
    lock(&REMOVALS).push(Removal { id, r#async });
    if r#async {
        return;
    }
    let entry = {
        let mut held = lock(&HELD);
        held.iter()
            .position(|(held_id, _)| *held_id == id)
            .map(|position| held.remove(position))
    };
    if let Some((_, removed)) = entry {
        run_removal(id, removed, 0);
    }
}

unsafe extern "C" fn mock_get_context_ptr(_this: *mut IKHook) -> *mut c_void {
    THREAD_CONTEXT.with(|stack| {
        stack
            .borrow()
            .last()
            .map_or(ptr::null_mut(), |&context| context as *mut c_void)
    })
}

unsafe extern "C" fn mock_get_original_function(_this: *mut IKHook) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn mock_get_original_value_ptr(_this: *mut IKHook) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn mock_get_override_value_ptr(_this: *mut IKHook) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn mock_get_current_value_ptr(_this: *mut IKHook, _pop: bool) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn mock_destroy_return_value(_this: *mut IKHook) {}

unsafe extern "C" fn mock_find_original(_this: *mut IKHook, function: *mut c_void) -> *mut c_void {
    function
}

unsafe extern "C" fn mock_find_original_virtual(
    _this: *mut IKHook,
    vtable: *mut *mut c_void,
    index: i32,
) -> *mut c_void {
    // SAFETY: the caller guarantees the slot is readable.
    unsafe { *vtable.offset(index as isize) }
}

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

unsafe extern "C" fn mock_lookup_signature(
    _this: *mut IKHook,
    _start: *mut c_void,
    _size: usize,
    _signature: *const c_char,
) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn mock_was_original_function_skipped(_this: *mut IKHook) -> bool {
    false
}

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

struct KhookObj(IKHook);

// SAFETY: the object's only field is a pointer to the immutable static table above.
unsafe impl Sync for KhookObj {}

static MOCK_KHOOK_OBJ: KhookObj = KhookObj(IKHook {
    vtable: ptr::addr_of!(MOCK_KHOOK_VTABLE).cast::<c_void>(),
});

// ---------------------------------------------------------------------------------------
// The fabricated server object
// ---------------------------------------------------------------------------------------

/// A stand-in for `ISource2Server::GameFrame`: it is never called (the mock KHook installs no
/// detour), so it only has to exist at the generated slot.
unsafe extern "C" fn server_game_frame(_this: *mut c_void, _a: u8, _b: u8, _c: u8) {}

/// The server's class table: every entry at the generated length, `GameFrame` at its generated
/// slot (`ISource2Server_slots::GameFrame`).
fn server_vtable() -> Box<[*const c_void; ISource2Server_slots::VTLEN]> {
    let mut table = Box::new([ptr::null::<c_void>(); ISource2Server_slots::VTLEN]);
    table[ISource2Server_slots::GameFrame] = server_game_frame as *const c_void;
    table
}

// ---------------------------------------------------------------------------------------
// The mock ISmmAPI
// ---------------------------------------------------------------------------------------

/// What the mock factory answers. One case per refusal in `gameframe::install`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Factory {
    /// `GetServerFactory` answers a factory and `VInterfaceMatch` the server object.
    Server,
    /// `GetServerFactory(true)` answers null.
    Null,
    /// `VInterfaceMatch` answers null.
    NoInterface,
}

static FACTORY: Mutex<Factory> = Mutex::new(Factory::Server);
static SERVER_OBJECT: AtomicUsize = AtomicUsize::new(0);
/// The interface name `VInterfaceMatch` was asked for (must be the generated version string).
static MATCH_NAME: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static MATCH_MIN: AtomicUsize = AtomicUsize::new(usize::MAX);
static FACTORY_SYN: AtomicUsize = AtomicUsize::new(usize::MAX);
static ADD_LISTENERS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn mock_get_detour_interface(_this: *mut ISmmAPI, _id: c_int) -> *mut c_void {
    ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut().cast::<c_void>()
}

unsafe extern "C" fn mock_add_listener(
    _this: *mut ISmmAPI,
    _plugin: *mut ISmmPlugin,
    _listener: *mut s2sdk_sys::generated::IMetamodListener,
) {
    ADD_LISTENERS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn mock_get_server_factory(_this: *mut ISmmAPI, syn: bool) -> *const c_void {
    FACTORY_SYN.store(usize::from(syn), Ordering::SeqCst);
    match *lock(&FACTORY) {
        Factory::Null => ptr::null(),
        // A non-null `CreateInterfaceFn`; the mock never calls it, `VInterfaceMatch` answers.
        Factory::Server | Factory::NoInterface => 0x1000usize as *const c_void,
    }
}

unsafe extern "C" fn mock_vinterface_match(
    _this: *mut ISmmAPI,
    factory: *const c_void,
    iface: *const c_char,
    min: i32,
) -> *mut c_void {
    check(!factory.is_null(), || {
        "VInterfaceMatch with a null factory".to_owned()
    });
    // SAFETY: Metamod's caller passes a NUL-terminated interface name.
    let name = unsafe { core::ffi::CStr::from_ptr(iface) };
    *lock(&MATCH_NAME) = name.to_bytes().to_vec();
    MATCH_MIN.store(min as usize, Ordering::SeqCst);
    match *lock(&FACTORY) {
        Factory::Server => SERVER_OBJECT.load(Ordering::SeqCst) as *mut c_void,
        Factory::Null | Factory::NoInterface => ptr::null_mut(),
    }
}

/// An `ISmmAPI` whose factory slots are filled. `Server` is the one every install test uses;
/// `Null`/`NoInterface` are the refusals.
struct SmmTable(ISmmAPI_vtable);

// SAFETY: the table holds only function addresses; nothing writes it.
unsafe impl Sync for SmmTable {}

static SMM_VTABLE: SmmTable = SmmTable(ISmmAPI_vtable {
    GetDetourInterface: Some(mock_get_detour_interface),
    AddListener: Some(mock_add_listener),
    GetServerFactory: Some(mock_get_server_factory),
    VInterfaceMatch: Some(mock_vinterface_match),
    ..ISmmAPI_vtable::NULL
});

struct SmmObj(ISmmAPI);

// SAFETY: the object's only field is a pointer to the immutable static table above.
unsafe impl Sync for SmmObj {}

static SMM_OBJ: SmmObj = SmmObj(ISmmAPI {
    vtable: ptr::addr_of!(SMM_VTABLE.0).cast::<c_void>(),
});

fn ismm() -> *mut ISmmAPI {
    ptr::addr_of!(SMM_OBJ.0).cast_mut()
}

/// An `ISmmAPI` with no factory slots at all (a broken Metamod):
/// `gameframe::install` must refuse rather than guess.
struct NoFactoryTable(ISmmAPI_vtable);

// SAFETY: as for `SmmTable`.
unsafe impl Sync for NoFactoryTable {}

static NO_FACTORY_VTABLE: NoFactoryTable = NoFactoryTable(ISmmAPI_vtable {
    GetDetourInterface: Some(mock_get_detour_interface),
    AddListener: Some(mock_add_listener),
    ..ISmmAPI_vtable::NULL
});

struct NoFactoryObj(ISmmAPI);

// SAFETY: as for `SmmObj`.
unsafe impl Sync for NoFactoryObj {}

static NO_FACTORY_OBJ: NoFactoryObj = NoFactoryObj(ISmmAPI {
    vtable: ptr::addr_of!(NO_FACTORY_VTABLE.0).cast::<c_void>(),
});

// ---------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------

/// Serialises the cases: `s2sdk::hook`'s API slot and the bridge's own state are process-wide.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Writes the bridge's log to `target/scratch/<name>.log` for the case's lifetime (never the
/// user's `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log`), like the lib's `hooks::test_guard`.
struct ScratchLog {
    path: std::path::PathBuf,
    previous: Option<String>,
}

impl ScratchLog {
    fn new(name: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&dir).expect("target/scratch exists");
        let path = dir.join(format!("gameframe-{name}.log"));
        let _ = std::fs::remove_file(&path);
        let previous = std::env::var("CS2_BRIDGE_LOG").ok();
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        Self { path, previous }
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    fn count(&self, needle: &str) -> usize {
        self.text().lines().filter(|line| line.contains(needle)).count()
    }
}

impl Drop for ScratchLog {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("CS2_BRIDGE_LOG", value),
            None => std::env::remove_var("CS2_BRIDGE_LOG"),
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Run one case with the mock KHook installed, a scratch log and the process left as found.
fn with_bridge<R>(name: &str, body: impl FnOnce(&ScratchLog) -> R) -> R {
    let _guard = lock(&TEST_LOCK);
    // A case that panicked skips its own cleanup: start every case from "nothing installed".
    let _ = gameframe::teardown();
    let _ = hook::remove_all_blocking(std::time::Duration::from_secs(5));
    hook::uninstall();
    reset_mock();
    *lock(&FACTORY) = Factory::Server;
    lock(&MATCH_NAME).clear();
    MATCH_MIN.store(usize::MAX, Ordering::SeqCst);
    FACTORY_SYN.store(usize::MAX, Ordering::SeqCst);
    ADD_LISTENERS.store(0, Ordering::SeqCst);
    let _ = take_violations();

    // The server object lives for the case; its vptr is the class table above.
    let table = server_vtable();
    let object = Box::leak(Box::new(ISource2Server {
        vtable: table.as_ptr().cast::<s2sdk_sys::generated::ISource2Server_vtable>(),
    }));
    let object_ptr = object as *mut ISource2Server as *mut c_void;
    SERVER_OBJECT.store(object_ptr as usize, Ordering::SeqCst);

    let scratch = ScratchLog::new(name);
    // SAFETY: `MOCK_KHOOK_OBJ` is this file's static `IKHook`, live for the process.
    unsafe { cs2_bridge::khook_api::install(ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut()) };
    // The plugin's "already loaded" state is process-wide: a case that panicked before its own
    // `Unload` must not make every later `Load` refuse. Tear a leftover role down with the mock
    // API (a test-only recovery; a real image never does this).
    if cs2_bridge::plugin::is_present() {
        let _ = call_unload(plugin_object());
    }
    let out = body(&scratch);

    // Teardown: the hook (if any) is dropped, the drain runs its removal, the API is forgotten.
    let _ = gameframe::teardown();
    if cs2_bridge::plugin::is_present() {
        let _ = call_unload(plugin_object());
    }
    let _ = hook::remove_all_blocking(std::time::Duration::from_secs(10));
    hook::uninstall();
    let violations = take_violations();
    assert!(
        violations.is_empty(),
        "the mock APIs recorded contract violations: {violations:#?}"
    );
    drop(table);
    out
}

/// Drive the `pre` thunk the mock was given, as KHook's trampoline does.
fn drive_pre(setup: VirtualSetup) {
    THREAD_CONTEXT.with(|stack| stack.borrow_mut().push(setup.context));
    // SAFETY: `setup.pre` is the `pre` thunk `khook_thunks!` generated for
    // `void f(void*, u8, u8, u8)` and this file registered it (`khook.hpp:2187`).
    unsafe {
        let pre: unsafe extern "C" fn(*mut c_void, u8, u8, u8) = core::mem::transmute(setup.pre);
        pre(ptr::null_mut(), 1, 0, 0);
    }
    THREAD_CONTEXT.with(|stack| stack.borrow_mut().pop());
}

/// Drive the `post` thunk the mock was given, with the hook's context pushed.
fn drive_post(setup: VirtualSetup) {
    THREAD_CONTEXT.with(|stack| stack.borrow_mut().push(setup.context));
    // SAFETY: `setup.post` is the `post` thunk `khook_thunks!` generated for
    // `void f(void*, u8, u8, u8)` and this file registered it (`khook.hpp:2187`).
    unsafe {
        let post: unsafe extern "C" fn(*mut c_void, u8, u8, u8) = core::mem::transmute(setup.post);
        post(ptr::null_mut(), 1, 0, 0);
    }
    THREAD_CONTEXT.with(|stack| stack.borrow_mut().pop());
}

/// Drive the `post` thunk from a thread that is not the one that installed it - the call
/// engine2's `"ServerGameFrame"` pool job makes when `sv_tick_parallel_with_client` is on. The
/// hook's context is pushed on that thread, as KHook's trampoline does.
fn drive_post_on_another_thread(setup: VirtualSetup) {
    std::thread::spawn(move || {
        THREAD_CONTEXT.with(|stack| stack.borrow_mut().push(setup.context));
        // SAFETY: as in `drive_post`; the thunk reads the context from this thread's stack.
        unsafe {
            let post: unsafe extern "C" fn(*mut c_void, u8, u8, u8) =
                core::mem::transmute(setup.post);
            post(ptr::null_mut(), 1, 0, 0);
        }
        THREAD_CONTEXT.with(|stack| stack.borrow_mut().pop());
    })
    .join()
    .expect("the foreign frame thread finished");
}

/// The latest registration the mock saw (a re-`Load` installs a second hook).
fn latest_setup() -> VirtualSetup {
    *setups().last().expect("one registration per Load")
}

// ---------------------------------------------------------------------------------------
// The Metamod lifecycle, through the plugin's own vtable
// ---------------------------------------------------------------------------------------

/// The plugin object, as `CreateInterface("ISmmPlugin")` answers it.
fn plugin_object() -> *mut ISmmPlugin {
    cs2_bridge::plugin::object().cast()
}

/// One entry of the plugin object's table, read the way Metamod reads it.
unsafe fn plugin_slot(object: *mut ISmmPlugin, slot: usize) -> *const c_void {
    // SAFETY: `object` is this crate's static plugin object, whose first word is its vtable.
    let vtable = unsafe { (object.cast::<*const *const c_void>()).read() };
    // SAFETY: `slot` is a generated `ISmmPlugin_slots` index inside that table.
    unsafe { *vtable.add(slot) }
}

/// `ISmmPlugin::Load` (`core/ISmmPlugin.h:107`), called through the object's table.
fn call_load(object: *mut ISmmPlugin, ismm: *mut ISmmAPI) -> (bool, String) {
    // the test binary has no -insecure on its command line (cs2_bridge::vac)
    cs2_bridge::vac::allow_secure_for_tests();
    let mut error = [0 as c_char; 256];
    // SAFETY: the slot is the generated `Load` and the function pointer has the generated
    // signature (`plugin::load`); `error` is writable for its length.
    let ok = unsafe {
        let load: unsafe extern "C" fn(
            *mut ISmmPlugin,
            c_int,
            *mut ISmmAPI,
            *mut c_char,
            usize,
            bool,
        ) -> bool = core::mem::transmute(plugin_slot(object, ISmmPlugin_slots::Load));
        load(object, 7, ismm, error.as_mut_ptr(), error.len(), false)
    };
    (ok, cstr(&error))
}

/// `ISmmPlugin::Unload` (`core/ISmmPlugin.h:154`), called through the object's table.
fn call_unload(object: *mut ISmmPlugin) -> (bool, String) {
    let mut error = [0 as c_char; 256];
    // SAFETY: as in `call_load`; the slot is the generated `Unload`.
    let ok = unsafe {
        let unload: unsafe extern "C" fn(*mut ISmmPlugin, *mut c_char, usize) -> bool =
            core::mem::transmute(plugin_slot(object, ISmmPlugin_slots::Unload));
        unload(object, error.as_mut_ptr(), error.len())
    };
    (ok, cstr(&error))
}

fn cstr(buf: &[c_char]) -> String {
    // SAFETY: `set_error` writes a NUL-terminated string; an all-zero buffer is empty.
    unsafe { core::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// `$XDG_RUNTIME_DIR` for the case's lifetime: the scratch directory the listener writes
/// `cs2mcpbridge/session.json` into (never the user's real runtime dir).
struct XdgGuard {
    previous: Option<std::ffi::OsString>,
    dir: PathBuf,
}

impl XdgGuard {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch XDG directory exists");
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        Self { previous, dir }
    }

    fn discovery(&self) -> PathBuf {
        self.dir.join("cs2mcpbridge").join("session.json")
    }
}

impl Drop for XdgGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }
}

/// Waits for `condition` with a deadline, then fails naming it.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

/// One quoted string field of the discovery file (`"token": "..."`).
fn quoted_field(text: &str, field: &str) -> String {
    let marker = format!("\"{field}\": \"");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("{field} in {text}"))
        + marker.len();
    let rest = &text[start..];
    rest[..rest.find('"').expect("the field's closing quote")].to_string()
}

/// One unquoted number field of the discovery file (`"port": 42683`).
fn number_field(text: &str, field: &str) -> u64 {
    let marker = format!("\"{field}\": ");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("{field} in {text}"))
        + marker.len();
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .expect("the number's end");
    rest[..end].parse().expect("a decimal number")
}

/// `control.hello` over a fresh loopback connection: a served image answers the capabilities.
fn hello(port: u16, token: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the listener accepts");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    writeln!(
        stream,
        "{{\"jsonrpc\":\"2.0\",\"id\":\"h1\",\"method\":\"control.hello\",\
         \"params\":{{\"protocol\":1,\"token\":\"{token}\"}}}}"
    )
    .expect("the hello is written");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("the hello's answer");
    line
}

/// The bridge's own thread names in this process (`/proc/self/task/*/comm`), for the
/// "the listener's threads are joined" assertion. Empty off Linux.
fn live_thread_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc/self/task") {
        for entry in entries.flatten() {
            if let Ok(name) = std::fs::read_to_string(entry.path().join("comm")) {
                names.push(name.trim().to_string());
            }
        }
    }
    names
}

/// A request for the drain to find; the method is the dispatch tail's (`-32601`), which is a
/// complete answer and needs no engine.
fn a_request() -> Request {
    Request {
        id: "gameframe-tick".to_string(),
        method: "gameframe.tick".to_string(),
        params: None,
        received: std::time::Instant::now(),
        deadline: None,
        gen: 0,
    }
}

// ---------------------------------------------------------------------------------------
// The hook
// ---------------------------------------------------------------------------------------

/// `install` hooks exactly the generated slot on the server object's own vptr, asynchronously,
/// with a post callback - and the status line says the tick source is `"gameframe"`.
#[test]
fn install_hooks_the_generated_slot_on_the_objects_own_vptr() {
    with_bridge("install", |_scratch| {
        // SAFETY: the mock API is live for the case and this is the object Metamod would hand
        // `Load`.
        unsafe { gameframe::install(ismm()) }.expect("the mock factory answers a server object");

        let setup = the_setup();
        // SAFETY: `SERVER_OBJECT` is the fabricated `ISource2Server` the mock factory answered,
        // live for the case; its first word is its vptr.
        let object_vptr = unsafe { (SERVER_OBJECT.load(Ordering::SeqCst) as *const usize).read() };
        assert_eq!(
            setup.vtable, object_vptr,
            "the table is the object's own vptr (its first word is the class table)"
        );
        assert_eq!(
            setup.index,
            ISource2Server_slots::GameFrame as i32,
            "the generated slot, cast to i32"
        );
        assert!(setup.r#async, "SetupMode::Async reaches KHook as async = true");
        assert!(setup.post != 0, "the post callback is registered");
        assert!(setup.pre != 0, "the frame-depth pre callback is registered");
        // `GetServerFactory(true)` - the synthetic wrapper Metamod's `GET_V_IFACE_ANY` asks for
        // (`core/ISmmPlugin.h:585`).
        assert_eq!(FACTORY_SYN.load(Ordering::SeqCst), 1, "syn = true");
        assert_eq!(
            lock(&MATCH_NAME).as_slice(),
            INTERFACEVERSION_SERVERGAMEDLL.as_bytes(),
            "the generated interface version"
        );
        assert_eq!(MATCH_MIN.load(Ordering::SeqCst), 0, "min = 0: any version");
        assert!(gameframe::installed());
        assert_eq!(gameframe::tick_source(), "gameframe");
        assert_eq!(gameframe::state(), "installed");
    });
}

/// A second `install` on the same image is a no-op: one registration, one hook.
#[test]
fn install_is_idempotent() {
    with_bridge("install-twice", |_scratch| {
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("first install");
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("second install is a no-op");
        assert_eq!(setups().len(), 1, "one SetupVirtualHook");
    });
}

/// Driving the `post` thunk ticks: the queue drain runs, the counter counts, and the log says
/// `gameframe tick 1` (the tools tick's own rule).
#[test]
fn the_post_callback_ticks_the_bridge_on_a_host_without_the_tools_framework() {
    with_bridge("tick", |scratch| {
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("install");
        let setup = the_setup();
        let before = gameframe::ticks();

        // A request nothing else will answer: if the post runs `wiring::tick`, the drain takes
        // it out of the queue.
        queue::global()
            .push(a_request())
            .expect("the queue has room");
        assert_eq!(queue::global().depth(), 1, "the request is queued");

        drive_pre(setup);
        drive_post(setup);

        assert_eq!(gameframe::ticks(), before + 1, "the callback counted one tick");
        assert_eq!(
            queue::global().depth(),
            0,
            "wiring::tick's drain ran: the queued request is gone"
        );
        // The counter is process-wide, so the number of this tick is `before + 1`; only its
        // power-of-two values are logged (the tools tick's rule).
        let line = format!("gameframe tick {}", before + 1);
        let logged = scratch.count(&line);
        assert_eq!(
            logged,
            usize::from((before + 1).is_power_of_two()),
            "{line} logging for this tick: {}",
            scratch.text()
        );
        assert!(
            scratch.text().lines().all(|entry| entry.starts_with("[cs2-bridge ")),
            "every line carries the role tag: {}",
            scratch.text()
        );
    });
}

/// Only the powers of two log, exactly as the tools tick does (`toolsys::run_frame`).
#[test]
fn only_powers_of_two_log() {
    with_bridge("powers", |scratch| {
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("install");
        let setup = the_setup();
        let before = gameframe::ticks();
        for _ in 0..5 {
            drive_pre(setup);
            drive_post(setup);
        }
        assert_eq!(gameframe::ticks(), before + 5);
        let text = scratch.text();
        let logged: Vec<String> = text
            .lines()
            .filter_map(|line| line.split("gameframe tick ").nth(1))
            .map(str::to_string)
            .collect();
        let expected: Vec<String> = (before + 1..=before + 5)
            .filter(|n| n.is_power_of_two())
            .map(|n| n.to_string())
            .collect();
        assert_eq!(logged, expected, "{text}");
    });
}

/// A re-entrant `post` (a tick already running on this thread) does nothing: no second tick,
/// no nested drain.
#[test]
fn a_reentrant_post_does_not_tick() {
    with_bridge("reentrant", |scratch| {
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("install");
        let setup = the_setup();
        let before = gameframe::ticks();

        // Hold the claim the callback takes, then enter the callback as a nested call would.
        let held = gameframe::enter_tick().expect("the thread starts outside a tick");
        drive_pre(setup);
        drive_post(setup);
        assert_eq!(
            gameframe::ticks(),
            before,
            "a nested entry must not run the tick: {}",
            scratch.text()
        );
        drop(held);

        // And the same callback ticks once the claim is free again.
        drive_pre(setup);
        drive_post(setup);
        assert_eq!(gameframe::ticks(), before + 1);
    });
}

/// The `pre` callback marks the thread inside the frame and the `post` callback clears it: the
/// state an unload from inside the frame is refused on (`plugin::unload`).
#[test]
fn the_frame_depth_is_visible_between_pre_and_post() {
    with_bridge("frame-depth", |_scratch| {
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("install");
        let setup = the_setup();
        assert!(!gameframe::in_frame(), "nothing is running yet");
        drive_pre(setup);
        assert!(gameframe::in_frame(), "the engine's frame body is on this stack");
        drive_post(setup);
        assert!(!gameframe::in_frame(), "the post callback lowered the depth");
    });
}

/// The teardown only **queues** the removal (`Hook`'s asynchronous `Drop`) and never asks for a
/// synchronous one: the removal can run on `GameFrame`'s own stack (`meta unload` from the
/// console), where a synchronous removal would spin for the capsule this thread holds shared.
/// **Negative:** a `RemoveHook(id, async = false)` for this hook before the drain fails the case.
#[test]
fn teardown_queues_an_asynchronous_removal_only() {
    with_bridge("teardown", |_scratch| {
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("install");
        let id = SETUPS.with_id();
        let report = gameframe::teardown();
        assert_eq!(report.owner, "gameframe");
        assert!(report.detail.contains("asynchronous"), "{}", report.detail);
        assert!(!gameframe::installed());
        assert!(gameframe::state().starts_with("not installed: torn down"), "{}", gameframe::state());

        let seen = removals();
        assert_eq!(seen.len(), 1, "one removal: {seen:?}");
        assert_eq!(seen[0].id, id);
        assert!(
            seen[0].r#async,
            "the teardown must ask for KHook's asynchronous removal, never remove_sync"
        );

        // The registry entry is idempotent.
        let second = gameframe::teardown();
        assert_eq!(second.detail, "not armed (no-op)");
        assert_eq!(removals().len(), 1, "nothing more was asked of KHook");
    });
}

/// The `GameFrame` hook's teardown entry is in the shared registry (`hooks::hooks_teardown`), so
/// both lifecycle ends drop it.
#[test]
fn the_hook_is_in_the_teardown_registry() {
    with_bridge("registry", |_scratch| {
        // SAFETY: as above.
        unsafe { gameframe::install(ismm()) }.expect("install");
        cs2_bridge::hooks::hooks_teardown();
        assert!(!gameframe::installed(), "the registry dropped the hook");
        assert!(removals().iter().all(|removal| removal.r#async));
    });
}

/// The refusals: each answers `Err`, installs nothing and makes the status say why.
#[test]
fn a_refused_install_installs_nothing_and_says_why() {
    // `GetServerFactory` answers null.
    with_bridge("no-factory", |_scratch| {
        *lock(&FACTORY) = Factory::Null;
        // SAFETY: as above.
        let error = unsafe { gameframe::install(ismm()) }.expect_err("no factory, no hook");
        assert_eq!(error, gameframe::InstallError::NoServerFactory);
        assert!(setups().is_empty(), "nothing was registered");
        assert!(!gameframe::installed());
        assert_eq!(gameframe::tick_source(), "none");
        assert_eq!(
            gameframe::state(),
            "unavailable: GetServerFactory answered null"
        );
    });

    // `VInterfaceMatch` answers null: no `Source2Server001` in this process.
    with_bridge("no-interface", |_scratch| {
        *lock(&FACTORY) = Factory::NoInterface;
        // SAFETY: as above.
        let error = unsafe { gameframe::install(ismm()) }.expect_err("no object, no hook");
        assert_eq!(error, gameframe::InstallError::NoServerObject);
        assert!(setups().is_empty());
        assert_eq!(gameframe::tick_source(), "none");
        assert!(gameframe::state().contains("Source2Server001"));
    });

    // `ISmmAPI` without the two slots (a broken Metamod).
    with_bridge("no-slots", |_scratch| {
        // SAFETY: this is a live `ISmmAPI` object this file owns; both slots are null.
        let error = unsafe {
            gameframe::install(ptr::addr_of!(NO_FACTORY_OBJ.0).cast_mut())
        }
        .expect_err("no slots, no hook");
        assert_eq!(error, gameframe::InstallError::SlotsMissing);
        assert!(setups().is_empty());
    });

    // KHook refuses the registration (`INVALID_HOOK`).
    with_bridge("refused", |_scratch| {
        FAIL_SETUP.store(true, Ordering::SeqCst);
        // SAFETY: as above.
        let error = unsafe { gameframe::install(ismm()) }.expect_err("KHook refused");
        assert_eq!(error, gameframe::InstallError::HookRefused);
        assert!(!gameframe::installed());
        assert_eq!(gameframe::tick_source(), "none");
        assert!(gameframe::state().contains("KHook refused"));
    });
}

// ---------------------------------------------------------------------------------------
// The Metamod Load path
// ---------------------------------------------------------------------------------------

/// The generated slot this case pins: an in-table entry, named by the generator.
#[test]
fn the_generated_slot_is_in_the_table() {
    // Variables, not consts: the same numbers checked against the generated tables.
    let slot = ISource2Server_slots::GameFrame;
    let len = ISource2Server_slots::VTLEN;
    let setup = IKHook_slots::SetupVirtualHook;
    let factory = ISmmAPI_slots::GetServerFactory;
    let match_slot = ISmmAPI_slots::VInterfaceMatch;
    assert!(slot < len);
    assert_eq!(slot, 19);
    assert_eq!(setup, 1);
    assert_eq!(factory, 4);
    assert_eq!(match_slot, 18);
    assert_eq!(IKHook_slots::SetupVirtualHook, 1);
    assert_eq!(ISmmAPI_slots::GetServerFactory, 4);
    assert_eq!(ISmmAPI_slots::VInterfaceMatch, 18);
}

/// A tiny helper for the teardown case: the id the mock registered.
trait SetupId {
    fn with_id(&self) -> u32;
}

impl SetupId for Mutex<Vec<VirtualSetup>> {
    fn with_id(&self) -> u32 {
        lock(&HELD).first().map_or(0, |(id, _)| *id)
    }
}

// ---------------------------------------------------------------------------------------
// Round-2 review cases: the thread, the runtime teardown, the refused unload
// ---------------------------------------------------------------------------------------

/// A frame on a thread that is not the one `Load` ran on must not tick: engine2 can run the
/// server frame as a `"ServerGameFrame"` pool job (`sv_tick_parallel_with_client`), and the
/// bridge's tick is engine-thread work. The explanation is logged once, not once per frame.
#[test]
fn a_frame_on_another_thread_does_not_tick_and_says_so_once() {
    with_bridge("off-thread", |scratch| {
        // SAFETY: as in the hook tests below.
        unsafe { gameframe::install(ismm()) }.expect("install");
        let setup = the_setup();
        let before = gameframe::ticks();
        queue::global()
            .push(a_request())
            .expect("the queue has room");

        for _ in 0..3 {
            drive_post_on_another_thread(setup);
        }
        assert_eq!(
            gameframe::ticks(),
            before,
            "the callback must not tick off the Load thread"
        );
        assert_eq!(
            queue::global().depth(),
            1,
            "the queue was not drained: no tick ran"
        );
        assert_eq!(
            scratch.count("GameFrame off the main thread"),
            1,
            "the explanation is logged once: {}",
            scratch.text()
        );
        // The status line must not present the source as healthy: the hook is still the source
        // (`"tickSource":"gameframe"`), and the off-thread flag says it is not producing ticks.
        assert!(
            gameframe::off_thread(),
            "the off-thread frame is recorded for the status line"
        );
        let status = iface::status_json();
        assert!(
            status.contains(r#""tickSource":"gameframe""#),
            "the hook is still the source: {status}"
        );
        assert!(
            status.contains(r#""gameframeOffThread":true"#),
            "the status line says the source is not producing ticks: {status}"
        );

        // The Load thread still ticks.
        drive_pre(setup);
        drive_post(setup);
        assert_eq!(gameframe::ticks(), before + 1);
        assert_eq!(queue::global().depth(), 0, "the tick drained the queue");
    });
}

/// A Metamod-only `Load` starts serving, `Unload` stops the listener (its threads joined, the
/// discovery file gone), and a **second** `Load` in the same process serves again: the queue's
/// shutdown and the "listener already started" one-shot are reset for the new life.
#[test]
fn a_metamod_only_unload_stops_the_listener_and_a_reload_serves_again() {
    with_bridge("lifecycle", |scratch| {
        let xdg = XdgGuard::new("gameframe-lifecycle-xdg");
        let discovery = xdg.discovery();
        let object = plugin_object();

        let (ok, error) = call_load(object, ismm());
        assert!(ok, "Load: {error}");
        assert!(cs2_bridge::plugin::is_present());
        assert_eq!(gameframe::tick_source(), "gameframe");

        // The first frame of an mm-only image is what starts the listener.
        let setup = the_setup();
        drive_pre(setup);
        drive_post(setup);
        wait_for("the listener to start", || cs2_bridge::wiring::port().is_some());
        let first_port = cs2_bridge::wiring::port().expect("the tick started the listener");
        wait_for("the discovery file", || discovery.exists());
        let first = std::fs::read_to_string(&discovery).expect("read session.json");
        assert_eq!(
            number_field(&first, "port"),
            u64::from(first_port),
            "session.json names the bound port"
        );
        let first_token = quoted_field(&first, "token");
        assert!(
            hello(first_port, &first_token).contains("\"capabilities\""),
            "the wire is served before the unload"
        );

        // Unload: the runtime Load started is stopped before Metamod dlcloses the image.
        let (ok, error) = call_unload(object);
        assert!(ok, "Unload: {error}");
        assert!(error.is_empty(), "{error}");
        assert!(!cs2_bridge::plugin::is_present());
        assert_eq!(
            cs2_bridge::wiring::port(),
            None,
            "the listener is stopped and dropped"
        );
        assert!(!discovery.exists(), "session.json is deleted (`:6799`)");
        assert!(
            TcpStream::connect_timeout(
                &(std::net::Ipv4Addr::LOCALHOST, first_port).into(),
                Duration::from_millis(200)
            )
            .is_err(),
            "nothing answers on the old port: the accept thread is gone"
        );
        let threads = live_thread_names();
        assert!(
            !threads.iter().any(|name| name.starts_with("cs2-bridge-ipc")
                || name.starts_with("cs2-bridge-writ")),
            "the listener's threads are joined, not left in this image: {threads:?}"
        );

        // A re-Load in the same process: the queue is reopened and the one-shot reset, so the
        // next frame starts a second listener and it serves.
        let (ok, error) = call_load(object, ismm());
        assert!(ok, "re-Load: {error}");
        let setup = latest_setup();
        drive_pre(setup);
        drive_post(setup);
        wait_for("the listener to start again", || {
            cs2_bridge::wiring::port().is_some()
        });
        let second_port = cs2_bridge::wiring::port().expect("the second tick started it");
        wait_for("the second discovery file", || discovery.exists());
        let second = std::fs::read_to_string(&discovery).expect("read session.json");
        let second_token = quoted_field(&second, "token");
        assert!(
            hello(second_port, &second_token).contains("\"capabilities\""),
            "the re-loaded image serves again: {}",
            scratch.text()
        );

        // The case ends unloaded, so the next `Load` in this binary starts from "absent".
        let (ok, error) = call_unload(object);
        assert!(ok, "the closing Unload: {error}");
    });
}

/// A refused `Unload` leaves the module verifier's stopping flag set - the unload cannot tell a
/// forced unload from an ordinary one, and a forced one gets no second chance to stop the
/// thread. A tick after it clears the flag (a tick proves the image is still mapped); that only
/// happens in an image that still has a tick, i.e. an mm+tool image or a later `Load`. In an
/// mm-only image the refusal has already dropped the GameFrame hook, so the plugin stays inert
/// until the `meta unload` retry succeeds. The tick here is called by the test directly.
#[test]
fn a_refused_unload_leaves_the_verifier_stopped_until_the_next_tick() {
    with_bridge("refused-unload", |scratch| {
        let object = plugin_object();
        let (ok, error) = call_load(object, ismm());
        assert!(ok, "Load: {error}");

        // Forget the KHook API under the live GameFrame hook: the unload's drain then has an id
        // to remove and no API to remove it with - `Error::KHookNotInstalled`, a refusal.
        hook::uninstall();

        let (ok, error) = call_unload(object);
        assert!(!ok, "the unload is refused, not carried on: {error}");
        assert!(
            error.contains("cannot remove the hooks"),
            "the refusal names the drain: {error}"
        );
        assert!(
            cs2_bridge::plugin::is_present(),
            "a refused unload keeps Metamod's plugin"
        );
        assert!(
            cs2_bridge::modules::live().verifier_stopping(),
            "the stopping flag stays set on a refusal"
        );

        // The next tick (a tools tick in an mm+tool image, or any later one) clears it.
        cs2_bridge::wiring::tick();
        assert!(
            !cs2_bridge::modules::live().verifier_stopping(),
            "a tick proves the image is mapped, so the verifier may run again: {}",
            scratch.text()
        );

        // Recover the test's state: with the API back, the pending removal is re-issued and the
        // second unload succeeds (`Hook`'s drop from inside `Drop` queues the id as pending).
        // SAFETY: the mock `IKHook` is this file's static, live for the process.
        unsafe { cs2_bridge::khook_api::install(ptr::addr_of!(MOCK_KHOOK_OBJ.0).cast_mut()) };
        let (ok, error) = call_unload(object);
        assert!(ok, "the retry unloads: {error}");
        assert!(!cs2_bridge::plugin::is_present());
    });
}
