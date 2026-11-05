//! Shared machinery for the two GameFrame test binaries (`tests/gameframe.rs` and
//! `tests/gameframe_tool.rs`): each uses a different subset, so the unused half is not dead code
//! in the crate as a whole.
#![allow(dead_code)]

//! Shared machinery for the two GameFrame test binaries: a mock `KHookApi`, a fake `ISmmAPI`
//! and a fake `ISource2Server`, plus the harness that installs one, drives the real post thunk
//! and leaves the process as it found it.
//!
//! The cases drive the **real generated thunk** (`game_frame`'s post) with the address the mock
//! `KHookApi` was given at registration - the call KHook's trampoline makes - so the callback,
//! the role check, the re-entrancy guard and the tick are all the production ones. The mock
//! records what KHook would do (`SetupVirtualHook`'s table/index/async flag, `RemoveHook`'s
//! async flag) and runs the removal function the wrapper registered, so the process-wide live
//! context count returns to zero.
//!
//! This binary never reaches the tools entry point, so its role set is exactly `mm`: the
//! tool-role case needs a binary where the tool role exists, which is `tests/gameframe_tool.rs`
//! (roles only ever grow in an image).
//!
//! The KHook API slot is process-wide, so the cases serialize on [`TEST_LOCK`].

use core::cell::RefCell;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use cs2_bridge::hooks;
use s2sdk::hook::{self, Action, HookId, KHookApi};
use s2sdk_sys::generated::{
    IKHook, IKHook_vtable, ISmmAPI, ISmmAPI_vtable, ISmmPlugin, ISmmPlugin_slots,
    INTERFACEVERSION_SERVERGAMEDLL, ISource2Server_slots,
};

// ---------------------------------------------------------------------------------------
// The mock KHookApi
// ---------------------------------------------------------------------------------------

/// One registration the mock saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub vtable: usize,
    pub index: i32,
    pub r#async: bool,
}

/// One removal the mock saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removal {
    pub id: HookId,
    pub r#async: bool,
}

pub struct Mock {
    setups: Mutex<Vec<Setup>>,
    removals: Mutex<Vec<Removal>>,
    /// `HookId` -> (the removal function, the context), as KHook stores them.
    held: Mutex<Vec<(HookId, usize, usize)>>,
    /// The post thunk the wrapper registered for the first hook (`RemoveHook`'s counterpart).
    post: AtomicUsize,
    /// The context of the first hook, for `GetContextPtr` while a case drives the thunk.
    context: AtomicUsize,
    /// Answer `INVALID_HOOK` instead of an id.
    pub fail: AtomicBool,
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

impl Mock {
    const fn new() -> Self {
        Self {
            setups: Mutex::new(Vec::new()),
            removals: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
            post: AtomicUsize::new(0),
            context: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn setups(&self) -> Vec<Setup> {
        Self::lock(&self.setups).clone()
    }

    pub fn removals(&self) -> Vec<Removal> {
        Self::lock(&self.removals).clone()
    }

    pub fn reset(&self) {
        Self::lock(&self.setups).clear();
        Self::lock(&self.removals).clear();
        Self::lock(&self.held).clear();
        self.post.store(0, Ordering::SeqCst);
        self.context.store(0, Ordering::SeqCst);
        self.fail.store(false, Ordering::SeqCst);
    }

    /// Run one held hook's removal function the way KHook does (`detour.cpp:1531-1541`): with
    /// the registering context pushed, so the wrapper's removal function releases it.
    fn run_removal(&self, id: HookId) {
        let entry = {
            let mut held = Self::lock(&self.held);
            match held.iter().position(|(held_id, _, _)| *held_id == id) {
                Some(index) => held.remove(index),
                None => return,
            }
        };
        let (_, removed, context) = entry;
        // SAFETY: `removed` is the address the wrapper registered as this signature's removal
        // thunk (`khook.hpp:711-719`), a `fn(HookId)` called with the context pushed.
        unsafe {
            CONTEXT.with(|stack| stack.borrow_mut().push(context));
            let f: unsafe extern "C" fn(HookId) = core::mem::transmute(removed);
            f(id);
            CONTEXT.with(|stack| stack.borrow_mut().pop());
        }
    }
}

thread_local! {
    /// KHook's per-thread context stack (`detour.cpp:359-361`), read by `GetContextPtr`.
    static CONTEXT: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

pub static MOCK: Mock = Mock::new();

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
        unreachable!("the GameFrame tick registers a virtual hook")
    }

    unsafe fn setup_virtual_hook(
        &self,
        vtable: *mut *mut c_void,
        index: i32,
        context: *mut c_void,
        removed_function: *mut c_void,
        _pre: *mut c_void,
        post: *mut c_void,
        _make_return: *mut c_void,
        _make_call_original: *mut c_void,
        _stack_size: u32,
        r#async: bool,
    ) -> HookId {
        assert!(!post.is_null(), "a post thunk is always registered");
        assert!(!removed_function.is_null(), "KHook needs the removal function");
        Mock::lock(&MOCK.setups).push(Setup {
            vtable: vtable as usize,
            index,
            r#async,
        });
        if MOCK.fail.load(Ordering::SeqCst) {
            return hook::INVALID_HOOK;
        }
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        MOCK.post.store(post as usize, Ordering::SeqCst);
        MOCK.context.store(context as usize, Ordering::SeqCst);
        Mock::lock(&MOCK.held).push((id, removed_function as usize, context as usize));
        id
    }

    unsafe fn remove_hook(
        &self,
        id: HookId,
        r#async: bool,
        _hook_removal_fn: *mut c_void,
        _context: *mut c_void,
    ) {
        Mock::lock(&MOCK.removals).push(Removal { id, r#async });
        // Both kinds run the removal function here: the case checks *which* one was asked for
        // (`async = false` from `remove_sync`/the drain, `true` from `Drop`), and running it
        // keeps the process-wide context count honest.
        MOCK.run_removal(id);
    }

    fn get_context_ptr(&self) -> *mut c_void {
        CONTEXT.with(|stack| {
            stack
                .borrow()
                .last()
                .map_or(ptr::null_mut(), |&context| context as *mut c_void)
        })
    }

    fn get_original_function(&self) -> *mut c_void {
        ptr::null_mut()
    }

    fn get_original_value_ptr(&self) -> *mut c_void {
        ptr::null_mut()
    }

    fn get_override_value_ptr(&self) -> *mut c_void {
        ptr::null_mut()
    }

    fn get_current_value_ptr(&self, _pop: bool) -> *mut c_void {
        ptr::null_mut()
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
        unreachable!("the wrapper never recalls a hook")
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
        unreachable!("the wrapper never looks a signature up")
    }

    fn was_original_function_skipped(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------------------
// A fake ISmmAPI and a fake ISource2Server
// ---------------------------------------------------------------------------------------

/// The server object: one vptr (the generated `ISource2Server` shape).
#[repr(C)]
pub struct Server {
    pub vt: *const *const c_void,
}

const SERVER_VT_LEN: usize = ISource2Server_slots::VTLEN;

/// The server's table: every entry the same stub (the plugin only reads the vptr and the slot).
struct ServerVtable([*const c_void; SERVER_VT_LEN]);

// SAFETY: a table of function addresses; nothing writes it.
unsafe impl Sync for ServerVtable {}

static SERVER_VTABLE: ServerVtable =
    ServerVtable([gameframe_stub as *const c_void; SERVER_VT_LEN]);

pub struct ServerCell(pub Server);

// SAFETY: the object is a pointer to the immutable `SERVER_VTABLE` static; nothing writes it.
unsafe impl Sync for ServerCell {}

pub static SERVER: ServerCell = ServerCell(Server {
    vt: ptr::addr_of!(SERVER_VTABLE.0).cast::<*const c_void>(),
});

/// The original `GameFrame` (never called: the mock does not build a trampoline).
unsafe extern "C" fn gameframe_stub(_this: *mut c_void, _a: u8, _b: u8, _c: u8) {}

/// What the fake `ISmmAPI`'s factory slots answer.
pub static FACTORY_NULL: AtomicBool = AtomicBool::new(false);
pub static OBJECT_NULL: AtomicBool = AtomicBool::new(false);

/// `ISmmAPI::GetServerFactory(bool)` (`core/ISmmAPI.h:127-134`): the mock factory pointer.
unsafe extern "C" fn get_server_factory(_this: *mut ISmmAPI, syn: bool) -> *const c_void {
    assert!(syn, "the header's `GET_V_IFACE_ANY` asks for the synthetic wrapper");
    if FACTORY_NULL.load(Ordering::SeqCst) {
        ptr::null()
    } else {
        mock_factory as *const c_void
    }
}

/// `ISmmAPI::VInterfaceMatch(fn, iface, min)` (`metamod.cpp:707-741`): our server object when
/// the name is the generated version, null otherwise.
unsafe extern "C" fn vinterface_match(
    _this: *mut ISmmAPI,
    factory: *const c_void,
    iface: *const core::ffi::c_char,
    min: i32,
) -> *mut c_void {
    assert_eq!(factory as usize, mock_factory as *const () as usize);
    assert_eq!(min, 0, "the ANY form of the macro searches from version 001");
    // SAFETY: the caller passes a NUL-terminated interface name (the plugin's own CString).
    let name = unsafe { core::ffi::CStr::from_ptr(iface) };
    assert_eq!(name.to_str().expect("ASCII"), INTERFACEVERSION_SERVERGAMEDLL);
    if OBJECT_NULL.load(Ordering::SeqCst) {
        ptr::null_mut()
    } else {
        ptr::addr_of!(SERVER.0).cast_mut().cast()
    }
}

/// The `CreateInterfaceFn` value `GetServerFactory` answers (never called).
unsafe extern "C" fn mock_factory(_name: *const core::ffi::c_char, _ret: *mut i32) -> *mut c_void {
    ptr::null_mut()
}

struct SmmVtable(ISmmAPI_vtable);

// SAFETY: a table of function addresses; nothing writes it.
unsafe impl Sync for SmmVtable {}

static SMM_VTABLE: SmmVtable = SmmVtable(ISmmAPI_vtable {
    GetServerFactory: Some(get_server_factory),
    VInterfaceMatch: Some(vinterface_match),
    GetDetourInterface: Some(get_detour_interface),
    AddListener: Some(add_listener),
    ..ISmmAPI_vtable::NULL
});

pub struct SmmCell(pub ISmmAPI);

// SAFETY: a pointer to the immutable table above.
unsafe impl Sync for SmmCell {}

pub static SMM: SmmCell = SmmCell(ISmmAPI {
    vtable: ptr::addr_of!(SMM_VTABLE.0).cast::<c_void>(),
});

// ---------------------------------------------------------------------------------------
// An IKHook table over `MOCK`, so a test can drive the plugin's `Load`/`Unload` path
// ---------------------------------------------------------------------------------------

/// `IKHook::SetupVirtualHook`, forwarded to the trait mock (`khook_vtable`'s adapter is the
/// production caller and forwards every argument verbatim).
unsafe extern "C" fn forward_setup_virtual_hook(
    _this: *mut IKHook,
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
) -> u32 {
    // SAFETY: the wrapper's setup contract; the mock records what KHook would.
    unsafe {
        MOCK.setup_virtual_hook(
            vtable,
            index,
            context,
            removed_function,
            pre,
            post,
            make_return,
            make_call_original,
            stack_size,
            r#async,
        )
    }
}

/// `IKHook::RemoveHook`, forwarded to the trait mock.
unsafe extern "C" fn forward_remove_hook(
    _this: *mut IKHook,
    id: HookId,
    r#async: bool,
    hook_removal_fn: *const c_void,
    context: *mut c_void,
) {
    // SAFETY: the wrapper's removal contract; a null callback is the one the wrapper passes.
    unsafe { MOCK.remove_hook(id, r#async, hook_removal_fn.cast_mut(), context) }
}

/// `IKHook::GetContextPtr`, forwarded to the trait mock (its per-thread stack).
unsafe extern "C" fn forward_get_context_ptr(_this: *mut IKHook) -> *mut c_void {
    MOCK.get_context_ptr()
}

/// `IKHook::FindOriginal`, forwarded to the trait mock.
unsafe extern "C" fn forward_find_original(
    _this: *mut IKHook,
    function: *mut c_void,
) -> *mut c_void {
    MOCK.find_original(function)
}

/// `IKHook::FindOriginalVirtual`, forwarded to the trait mock.
unsafe extern "C" fn forward_find_original_virtual(
    _this: *mut IKHook,
    vtable: *mut *mut c_void,
    index: i32,
) -> *mut c_void {
    MOCK.find_original_virtual(vtable, index)
}

/// The adapter's table: the slots this mock supports, the rest absent (the adapter reports a
/// missing slot instead of calling a null pointer).
static MOCK_IKHOOK_VTABLE: IKHook_vtable = IKHook_vtable {
    SetupVirtualHook: Some(forward_setup_virtual_hook),
    RemoveHook: Some(forward_remove_hook),
    GetContextPtr: Some(forward_get_context_ptr),
    FindOriginal: Some(forward_find_original),
    FindOriginalVirtual: Some(forward_find_original_virtual),
    ..IKHook_vtable::NULL
};

struct KhookObj(IKHook);

// SAFETY: a pointer to the immutable table above.
unsafe impl Sync for KhookObj {}

/// The `IKHook` object `GetDetourInterface` answers.
static MOCK_IKHOOK: KhookObj = KhookObj(IKHook {
    vtable: ptr::addr_of!(MOCK_IKHOOK_VTABLE).cast::<c_void>(),
});

/// `ISmmAPI::GetDetourInterface(id)` (`core/metamod.cpp:1081`).
unsafe extern "C" fn get_detour_interface(_this: *mut ISmmAPI, _id: core::ffi::c_int) -> *mut c_void {
    ptr::addr_of!(MOCK_IKHOOK.0).cast_mut().cast::<c_void>()
}

/// `ISmmAPI::AddListener(plugin, listener)` (`core/ISmmAPI.h:201`).
unsafe extern "C" fn add_listener(
    _this: *mut ISmmAPI,
    _plugin: *mut ISmmPlugin,
    _listener: *mut s2sdk_sys::generated::IMetamodListener,
) {
    ADD_LISTENERS.fetch_add(1, Ordering::SeqCst);
}

/// How many times the plugin's `Load` registered its listener.
pub static ADD_LISTENERS: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------------------
// The plugin object's `Load`/`Unload` through its generated table
// ---------------------------------------------------------------------------------------

/// The plugin object, as `CreateInterface("ISmmPlugin")` answers it.
pub fn plugin_object() -> *mut ISmmPlugin {
    cs2_bridge::plugin::object().cast()
}

unsafe fn plugin_slot(object: *mut ISmmPlugin, slot: usize) -> *const c_void {
    // SAFETY: `object` is this crate's static plugin object, whose first word is its vtable.
    let vtable = unsafe { (object.cast::<*const *const c_void>()).read() };
    // SAFETY: `slot` is a generated `ISmmPlugin_slots` index inside that table.
    unsafe { *vtable.add(slot) }
}

/// `ISmmPlugin::Load` (`core/ISmmPlugin.h:107`), through the object's table.
pub fn call_load(object: *mut ISmmPlugin, ismm: *mut ISmmAPI) -> (bool, String) {
    // the test binary has no -insecure on its command line (cs2_bridge::vac)
    cs2_bridge::vac::allow_secure_for_tests();
    let mut error = [0 as core::ffi::c_char; 256];
    // SAFETY: the slot is the generated `Load` with the generated signature.
    let ok = unsafe {
        let load: unsafe extern "C" fn(
            *mut ISmmPlugin,
            core::ffi::c_int,
            *mut ISmmAPI,
            *mut core::ffi::c_char,
            usize,
            bool,
        ) -> bool = core::mem::transmute(plugin_slot(object, ISmmPlugin_slots::Load));
        load(object, 7, ismm, error.as_mut_ptr(), error.len(), false)
    };
    (ok, c_str(&error))
}

/// `ISmmPlugin::Unload` (`core/ISmmPlugin.h:154`), through the object's table.
pub fn call_unload(object: *mut ISmmPlugin) -> (bool, String) {
    let mut error = [0 as core::ffi::c_char; 256];
    // SAFETY: as in `call_load`; the slot is the generated `Unload`.
    let ok = unsafe {
        let unload: unsafe extern "C" fn(
            *mut ISmmPlugin,
            *mut core::ffi::c_char,
            usize,
        ) -> bool = core::mem::transmute(plugin_slot(object, ISmmPlugin_slots::Unload));
        unload(object, error.as_mut_ptr(), error.len())
    };
    (ok, c_str(&error))
}

fn c_str(buf: &[core::ffi::c_char]) -> String {
    // SAFETY: `set_error` wrote a NUL-terminated string; an all-zero buffer is empty.
    unsafe { core::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------

/// The hook API slot and the plugin's own statics are process-wide.
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Run one case with a fresh mock installed, a scratch bridge log, and the process left as it
/// was: the hook torn down, the drain run, the API forgotten.
pub fn with_bridge<R>(name: &str, body: impl FnOnce(&'static Mock) -> R) -> R {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let log_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("gameframe-{name}.log"));
    std::fs::create_dir_all(log_path.parent().expect("target/scratch"))
        .expect("the scratch directory exists");
    let _ = std::fs::remove_file(&log_path);
    let previous_log = std::env::var("CS2_BRIDGE_LOG").ok();
    // SAFETY-free: `std::env::set_var` is safe on the supported toolchain; the test lock keeps
    // the process-wide environment single-threaded here (`--test-threads=4` runs other files in
    // other processes).
    std::env::set_var("CS2_BRIDGE_LOG", &log_path);

    assert_eq!(
        hook::live_context_count(),
        0,
        "a previous case left a hook context KHook can still call into"
    );
    MOCK.reset();
    FACTORY_NULL.store(false, Ordering::SeqCst);
    OBJECT_NULL.store(false, Ordering::SeqCst);
    ADD_LISTENERS.store(0, Ordering::SeqCst);
    hook::install(&MOCK);
    // The plugin's "already loaded" state is process-wide: a case that panicked before its own
    // `Unload` must not make every later `Load` refuse.
    if cs2_bridge::plugin::is_present() {
        let _ = call_unload(plugin_object());
    }
    let out = body(&MOCK);
    // The teardown path the unload uses: owners first, then the drain.
    hooks::hooks_teardown();
    if cs2_bridge::plugin::is_present() {
        let _ = call_unload(plugin_object());
    }
    let _ = hook::remove_all_blocking(std::time::Duration::from_secs(10));
    hook::uninstall();
    assert_eq!(
        hook::live_context_count(),
        0,
        "every removal function ran by the end of the case"
    );
    match &previous_log {
        Some(value) => std::env::set_var("CS2_BRIDGE_LOG", value),
        None => std::env::remove_var("CS2_BRIDGE_LOG"),
    }
    let _ = std::fs::remove_file(&log_path);
    out
}

/// Drive the registered post thunk the way KHook's trampoline does: the context pushed, the
/// hooked call's three `u8` flags.
pub fn drive_post(mock: &Mock) {
    let post = mock.post.load(Ordering::SeqCst);
    assert_ne!(post, 0, "the mock recorded a post thunk at registration");
    let context = mock.context.load(Ordering::SeqCst);
    // SAFETY: `post` is the thunk the wrapper generated for `void f(this, u8, u8, u8)` and
    // registered with this mock; KHook calls it with the hook's context pushed, which the
    // wrapper's `GetContextPtr` reads back.
    unsafe {
        CONTEXT.with(|stack| stack.borrow_mut().push(context));
        let thunk: unsafe extern "C" fn(*mut c_void, u8, u8, u8) = core::mem::transmute(post);
        thunk(ptr::addr_of!(SERVER.0).cast_mut().cast(), 1, 0, 0);
        CONTEXT.with(|stack| stack.borrow_mut().pop());
    }
}

pub fn log_text(name: &str) -> String {
    std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!("gameframe-{name}.log")),
    )
    .unwrap_or_default()
}

