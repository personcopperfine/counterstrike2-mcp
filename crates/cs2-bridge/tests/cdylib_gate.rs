//! Gate 1 of WP1.G: the cdylib a game would load, loaded here.
//!
//! This test does not link the rlib: it loads the cdylib the same build produced
//! (`libcs2_bridge.so` on Linux, `cs2_bridge.dll` on Windows) and resolves `CreateInterface`
//! from it, then walks the object it returns through the generated `ISmmPlugin_vtable` - the
//! path Metamod takes (`core/metamod_plugins.cpp:560`, `GetProcAddress`/`dlsym` for
//! `CreateInterface`, then `(pfn)(METAMOD_PLAPI_NAME, NULL)`).
//!
//! The library is the one cargo placed **next to the test executable**: `<profile>/deps/`
//! holds both the test binary and the cdylib of the library it links against, and cargo
//! rebuilds that cdylib as part of the same invocation. The copy one level up
//! (`<profile>/libcs2_bridge.so`) is only refreshed by `cargo build`, so `cargo test` after a
//! source change leaves it stale - measured: after touching `src/lib.rs`, `cargo test` wrote
//! `deps/libcs2_bridge.so` at 06:03:36 and left `<profile>/libcs2_bridge.so` at 06:00:20. The
//! gate therefore loads the `deps/` copy, and a missing file FAILS the test naming the path:
//! the gate is the built artifact of the code under test, not a reimplementation of it and not
//! an older build's file.
//!
//! The crate compiles on both targets; the only per-target expectation is that
//! `ToolSystem2_001` is served on Windows and refused elsewhere, which is `cfg!(windows)` in the
//! one case that asks for it.
//!
//! Both cases load the same file, so they share one image and its role set
//! (`log::Roles`, which only grows); the case that walks the entry points therefore does all of
//! its naming in one test, in a fixed order, and the other one calls nothing that adds a role.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use std::path::{Path, PathBuf};

use cs2_bridge::iface::{McpBridge, McpBridge_vtable};
use s2sdk_sys::generated::{ISmmPlugin, ISmmPlugin_slots, METAMOD_PLAPI_VERSION};

/// The generated plugin interface name, with the terminator Metamod's `strcmp` needs.
const PLAPI_NAME: &core::ffi::CStr = c"ISmmPlugin";
/// `McpBridge_001`.
const MCP_BRIDGE_NAME: &core::ffi::CStr = c"McpBridge_001";
/// `ToolSystem2_001`: the tools' name, which this target does not serve.
const TOOLSYSTEM2_NAME: &core::ffi::CStr = c"ToolSystem2_001";

/// `META_IFACE_OK` (`core/ISmmPlugin.h:54`).
const META_IFACE_OK: c_int = 0;
/// `META_IFACE_FAILED` (`core/ISmmPlugin.h:55`).
const META_IFACE_FAILED: c_int = 1;

/// The `CreateInterface` signature: `void *(*)(const char *, int *)`.
type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;

/// `ISmmPlugin::GetLogTag` (`core/ISmmPlugin.h:205`): `const char *GetLogTag()`.
type GetLogTagFn = unsafe extern "C" fn(*mut ISmmPlugin) -> *const c_char;

/// `ISmmPlugin::Load` (`core/ISmmPlugin.h:107`).
type LoadFn = unsafe extern "C" fn(
    *mut ISmmPlugin,
    c_int,
    *mut c_void,
    *mut c_char,
    usize,
    bool,
) -> bool;

/// The base address of the loaded image `addr` belongs to, or `None` when no loaded module
/// contains it.
///
/// There is no portable loader query, so the same check is written once per target: `dladdr`
/// on unix and `GetModuleHandleExW(FROM_ADDRESS)` on Windows. Neither is a "skip when absent"
/// path - `None` is an assertion failure at every call site below: every vtable entry (and the
/// table itself) must be inside the image that was just loaded.
#[cfg(unix)]
fn image_base(addr: usize) -> Option<usize> {
    let mut info: libc::Dl_info = unsafe { core::mem::zeroed() };
    // SAFETY: `dladdr` writes only into `info` and answers whether it found a module.
    let found = unsafe { libc::dladdr(addr as *const c_void, &mut info) };
    if found == 0 || info.dli_fbase.is_null() {
        None
    } else {
        Some(info.dli_fbase as usize)
    }
}

/// The Windows half of [`image_base`]: `GetModuleHandleExW` with `FROM_ADDRESS` answers the
/// module that contains `addr`, and `UNCHANGED_REFCOUNT` leaves its reference count alone.
/// The address is passed in the `lpModuleName` parameter, which is what the flag means.
#[cfg(windows)]
fn image_base(addr: usize) -> Option<usize> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    let mut module: HMODULE = ptr::null_mut();
    // SAFETY: `addr` is a code or data address in this process, and `module` is a writable
    // HMODULE; the flag pair asks for the containing module without taking a reference.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr as *const u16,
            &mut module,
        )
    };
    if found == 0 || module.is_null() {
        None
    } else {
        Some(module as usize)
    }
}

/// The cdylib this crate builds for the host, in the directory of the running test binary.
///
/// Cargo keeps the artifact of a library in `<profile>/deps/` next to the test executables that
/// link against it, and refreshes it on every `cargo build` and `cargo test`; the copy in
/// `<profile>/` is only refreshed by `cargo build` (see the module docs for the measurement).
/// So this executable's own directory is the one place whose file is always the code under
/// test. `libloading::library_filename` is the platform's own name for a library - the
/// `[lib] name = "cs2_bridge"` half of it - so the same rule finds `libcs2_bridge.so` on Linux
/// and `cs2_bridge.dll` on Windows.
fn cdylib_path() -> PathBuf {
    let exe = std::env::current_exe().expect("the running test binary's path");
    let deps = exe
        .parent()
        .expect("the test binary lives in cargo's deps/ directory");
    assert_eq!(
        deps.file_name().and_then(|n| n.to_str()),
        Some("deps"),
        "expected the test binary in cargo's deps/ directory, found {}",
        exe.display()
    );
    deps.join(libloading::library_filename("cs2_bridge"))
}

/// Load the cdylib and answer the library handle plus its `CreateInterface`.
///
/// # Safety
///
/// The returned library must outlive every pointer taken from it; the test keeps it in a local
/// that lives to the end of the case.
unsafe fn load_bridge() -> (libloading::Library, CreateInterfaceFn) {
    let path = cdylib_path();
    assert!(
        Path::new(&path).is_file(),
        "{} does not exist: this gate loads the cdylib cargo keeps beside the test binary in \
         deps/ (see `cdylib_path`), which the same cargo invocation that built this test binary \
         writes; the file is missing because this executable is not one cargo just ran, not \
         because a separate build step was skipped",
        path.display()
    );
    // SAFETY: the caller's contract; the library is a Rust cdylib built by this workspace.
    let lib = unsafe { libloading::Library::new(&path) }
        .unwrap_or_else(|e| panic!("dlopen {}: {e}", path.display()));
    // SAFETY: `CreateInterface` is the crate's one exported symbol with this signature
    // (`bridge/mcp_bridge.c:6846`), and `lib` is kept alive by the caller.
    let create: libloading::Symbol<'_, CreateInterfaceFn> =
        unsafe { lib.get(b"CreateInterface\0") }
            .unwrap_or_else(|e| panic!("dlsym CreateInterface in {}: {e}", path.display()));
    let create = *create;
    (lib, create)
}

/// Ask the loaded library for one name.
///
/// # Safety
///
/// `create` must come from [`load_bridge`]'s library, which must still be loaded.
unsafe fn create(create_fn: CreateInterfaceFn, name: &core::ffi::CStr) -> (*mut c_void, c_int) {
    let mut ret: c_int = -1;
    // SAFETY: the caller's contract; the name literal is NUL-terminated and `ret` is local.
    let object = unsafe { create_fn(name.as_ptr(), &mut ret) };
    (object, ret)
}

/// The `McpBridge_001` status line, read through the loaded object's own vtable.
///
/// The object is reached through the crate's own [`McpBridge`]: its `vt` field first, then
/// `version`, and the call through the table's `GetStatus` field - the struct's field order, not
/// a count of words (iface's unit tests pin the offsets of the C++ class the module docs draw).
/// The rlib compiled into this test binary and the cdylib it loads are two compilations of the
/// same source with the same profile, so the layout the struct describes is the layout the
/// loaded object has.
///
/// # Safety
///
/// `create_fn` must come from [`load_bridge`]'s library, which must still be loaded.
unsafe fn status_line(create_fn: CreateInterfaceFn) -> String {
    // SAFETY: the caller's contract.
    let (object, ret) = unsafe { create(create_fn, MCP_BRIDGE_NAME) };
    assert!(!object.is_null(), "the cdylib must serve McpBridge_001");
    assert_eq!(ret, META_IFACE_OK);

    // SAFETY: `object` is the `McpBridge` the library handed out, live for the process.
    let obj = object.cast::<McpBridge>();
    // SAFETY: as above.
    let version = unsafe { (*obj).version };
    assert_eq!(
        version,
        cs2_bridge::iface::VERSION,
        "the status object's version is the crate's `iface::VERSION`"
    );

    // SAFETY: as above; `vt` is the object's vtable pointer, the table the crate declares.
    let table: *const McpBridge_vtable = unsafe { (*obj).vt };
    assert!(!table.is_null(), "the object carries a vtable pointer");
    // SAFETY: `table` is that object's own table, whose `GetStatus` field is the entry a C++
    // `obj->GetStatus(buf, len)` call reaches; the signature comes from the crate's type.
    let get_status = unsafe { (*table).GetStatus };
    // v3's status line (roles + tick source + GameFrame state) is ~150 bytes.
    let mut buf = [0u8; 256];
    // SAFETY: `object` is the object whose table this entry came from - what a C++ virtual
    // call would pass as `this` - and the buffer is writable for its own length.
    let len = unsafe { get_status(obj, buf.as_mut_ptr(), buf.len()) };
    let text: Vec<u8> = buf.iter().take_while(|&&c| c != 0).copied().collect();
    let line = String::from_utf8(text).expect("the status line is ASCII");
    assert_eq!(len, line.len(), "the reported length is the line's");
    assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
    assert!(!line.contains('\n'), "{line}");
    line
}

/// Gate 1, in one image and one fixed order: the loaded cdylib's `CreateInterface` answers each
/// name it serves, the object behind `"ISmmPlugin"` is the generated `ISmmPlugin` (every
/// generated slot non-null and inside the loaded image, `GetApiVersion == 18`, `GetLogTag`, a
/// refused `Load`), and the status line says which entry point named this image.
///
/// # Why the entry points are walked in one test
///
/// `log::Roles` is per image and only grows, and every case in this file loads the *same*
/// `deps/` cdylib, so all of them share one image (and therefore one role set). Two cases adding
/// roles race under libtest's threads. The roles are therefore walked in one test, in a fixed
/// order, asserting the set before and after each entry point that adds one; the only other case
/// ([`loaded_cdylib_refuses_unknown_names_and_leaves_the_image_alone`]) calls names that neither
/// serve nor add a role, so it can run in parallel with this one.
#[test]
fn loaded_cdylib_serves_every_name_in_one_image_and_one_order() {
    scratch_log();
    // SAFETY: the library lives for the whole test; every pointer below comes from it.
    let (_lib, create_fn) = unsafe { load_bridge() };

    // The loader knows the image this build produced; `CreateInterface` is in it.
    let image = image_base(create_fn as usize)
        .expect("the loader must know the library the test just loaded");

    // Step 1: `McpBridge_001`, before any entry point has named this image.
    let line = unsafe { status_line(create_fn) };
    assert!(line.contains("\"metamod\":\"absent\""), "{line}");
    assert!(line.contains("\"khook\":false"), "{line}");
    assert!(
        line.contains("\"roles\":[]"),
        "nothing has named this image yet: {line}"
    );

    // Step 2: the tools framework's name. `cs2.exe -tools` resolves this object and drives it
    // every frame, and it is the entry point that names this image "tool". On this host it is
    // refused, and the tag stays "none".
    // SAFETY: as above.
    let (object, ret) = unsafe { create(create_fn, TOOLSYSTEM2_NAME) };
    if cfg!(windows) {
        assert!(!object.is_null(), "the tools framework's object on Windows");
        assert_eq!(ret, META_IFACE_OK);
    } else {
        assert!(object.is_null(), "no tool plugin on this target");
        assert_eq!(ret, META_IFACE_FAILED);
    }
    let after_tools = if cfg!(windows) { "[\"tool\"]" } else { "[]" };
    let line = unsafe { status_line(create_fn) };
    assert!(
        line.contains(&format!("\"roles\":{after_tools}")),
        "expected roles {after_tools}: {line}"
    );
    assert!(line.contains("\"metamod\":\"absent\""), "{line}");

    // Step 3: `ISmmPlugin`, the name Metamod resolves before it calls `Load`
    // (`core/metamod_plugins.cpp:560`, `:571`).
    // SAFETY: as above.
    let (object, ret) = unsafe { create(create_fn, PLAPI_NAME) };
    assert!(!object.is_null(), "the cdylib must serve ISmmPlugin");
    assert_eq!(ret, META_IFACE_OK);

    assert_eq!(
        core::mem::size_of::<ISmmPlugin>(),
        core::mem::size_of::<usize>(),
        "ISmmPlugin is one vtable pointer"
    );

    // The object's first word is the table the generated struct names (`vtable`); read it
    // through the generated field, never a hand-written offset.
    // SAFETY: `object` is the `ISmmPlugin` the library handed out, live for the process.
    let obj = object.cast::<ISmmPlugin>();
    let table = unsafe { (*obj).vtable };
    assert!(!table.is_null(), "the object carries a vtable pointer");
    assert_eq!(
        image_base(table as usize),
        Some(image),
        "the vtable must be inside the loaded image, not a copied or stale table"
    );

    // Walk the generated number of raw slots. Every one must be non-null and inside the image;
    // a table padded with nulls or pointing at the test's own copy would fail here.
    let entries = table.cast::<*const c_void>();
    assert_eq!(
        core::mem::size_of::<s2sdk_sys::generated::ISmmPlugin_vtable>(),
        ISmmPlugin_slots::VTLEN * core::mem::size_of::<usize>(),
        "the generated table has the generated per-ABI VTLEN"
    );
    for slot in 0..ISmmPlugin_slots::VTLEN {
        // SAFETY: the loaded object's table is the generated `ISmmPlugin_vtable`, and `slot`
        // is below its generated length.
        let entry = unsafe { *entries.add(slot) };
        assert!(!entry.is_null(), "ISmmPlugin slot {slot} is null");
        assert_eq!(
            image_base(entry as usize),
            Some(image),
            "ISmmPlugin slot {slot} is outside the loaded image"
        );
    }
    // The walk's evidence, for `cargo test -- --nocapture`.
    println!(
        "gate 1: image base {image:#x}, table {:#x}, {} slots (VTLEN) all non-null and in the image",
        table as usize,
        ISmmPlugin_slots::VTLEN
    );

    // Call the slots the way Metamod does. `GetApiVersion` is the first callback
    // (`core/metamod_plugins.cpp:585`, `core/ISmmPlugin.h:76`).
    // SAFETY: the slot at the generated index holds `GetApiVersion`
    // (`*mut ISmmPlugin -> i32`) in the loaded image.
    let get_api_version: unsafe extern "C" fn(*mut ISmmPlugin) -> c_int =
        unsafe { core::mem::transmute(*entries.add(ISmmPlugin_slots::GetApiVersion)) };
    // SAFETY: as above.
    assert_eq!(unsafe { get_api_version(obj) }, METAMOD_PLAPI_VERSION as c_int);
    assert_eq!(METAMOD_PLAPI_VERSION, 18);

    // `GetLogTag` (`core/ISmmPlugin.h:205`) is the tag Metamod prefixes our lines with, and it
    // must come from this image too (the string lives in its rodata).
    // SAFETY: the slot holds `GetLogTag` (`*mut ISmmPlugin -> *const c_char`).
    let get_log_tag: GetLogTagFn =
        unsafe { core::mem::transmute(*entries.add(ISmmPlugin_slots::GetLogTag)) };
    // SAFETY: the returned pointer is a static NUL-terminated string in the loaded image.
    let tag = unsafe { get_log_tag(obj) };
    assert!(!tag.is_null());
    // SAFETY: as above.
    assert_eq!(
        unsafe { core::ffi::CStr::from_ptr(tag) },
        c"cs2-mcp",
        "GetLogTag is the tag plugin.rs answers (`core/ISmmPlugin.h:205`)"
    );
    assert_eq!(image_base(tag as usize), Some(image), "the tag string is ours");

    // A `Load` with no `ISmmAPI` is refused through the loaded slot: false and the error filled
    // (`core/ISmmPlugin.h:107`; gate 3(b), here against the real artifact).
    // SAFETY: the slot holds `Load`; `this` is the object, `ismm` is null on purpose, `error`
    // is writable for its own length.
    let load: LoadFn = unsafe { core::mem::transmute(*entries.add(ISmmPlugin_slots::Load)) };
    let mut error = [0 as c_char; 128];
    // SAFETY: as above.
    let ok = unsafe { load(obj, 0, ptr::null_mut(), error.as_mut_ptr(), error.len(), true) };
    assert!(!ok, "Load without an ISmmAPI must be refused");
    let message: Vec<u8> = error.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    let message = String::from_utf8(message).expect("the error message is ASCII");
    assert!(message.contains("null ISmmAPI"), "{message}");

    // The role set after `ISmmPlugin`: the Metamod role was added by the entry point (the same
    // statement `Load` makes), and on Windows the tools role was already there - one image,
    // both roles (`log::Role`). "metamod" is still absent, because no `Load` has succeeded in
    // this image.
    let after_metamod = if cfg!(windows) {
        "[\"mm\",\"tool\"]"
    } else {
        "[\"mm\"]"
    };
    let line = unsafe { status_line(create_fn) };
    assert!(
        line.contains(&format!("\"roles\":{after_metamod}")),
        "expected roles {after_metamod}: {line}"
    );
    assert!(
        line.contains("\"metamod\":\"absent\""),
        "no Load has succeeded in this image: {line}"
    );
}

/// The names this target does not serve answer NULL with `*ret = META_IFACE_FAILED`, and a null
/// `ret` is not written. Nothing here names an image - an unknown name is refused before any
/// entry point runs, and `McpBridge_001` is no entry point - so this case cannot disturb the
/// order [`loaded_cdylib_serves_every_name_in_one_image_and_one_order`] depends on.
#[test]
fn loaded_cdylib_refuses_unknown_names_and_leaves_the_image_alone() {
    scratch_log();
    // SAFETY: the library lives for the whole test.
    let (_lib, create_fn) = unsafe { load_bridge() };

    // SAFETY: as above.
    let (object, ret) = unsafe { create(create_fn, c"Not_An_Interface") };
    assert!(object.is_null());
    assert_eq!(ret, META_IFACE_FAILED);

    // Metamod calls `CreateInterface(name, NULL)` when it only wants the object
    // (`core/metamod_plugins.cpp:571`).
    // SAFETY: a null `ret` is explicitly allowed.
    let object = unsafe { create_fn(MCP_BRIDGE_NAME.as_ptr(), ptr::null_mut()) };
    assert!(!object.is_null());
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
        let path = dir.join("cs2-bridge-cdylib-gate-test.log");
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        path
    })
    .clone()
}
