//! Windows: the injected loader DLL (`cs2mcp_mmload.dll`).
//!
//! `DllMain(DLL_PROCESS_ATTACH)` installs the hook and nothing else: no `LoadLibrary`, no thread,
//! no wait. The launcher injects this DLL into a *suspended* `cs2.exe` with
//! `CreateRemoteThread(LoadLibraryW)`, so whatever happens here happens while the process cannot
//! run a single instruction of its own, and the remote thread returns into the loader as soon as
//! this DLL's entry point returns.
//!
//! # What is installed
//!
//! Every kernel32 IAT slot whose current value is `kernelbase!LoadLibraryExW` (DESIGN §0: on Wine
//! `kernel32!LoadLibraryExW` is a thunk into kernelbase, so this slot is what every in-process
//! caller of the kernel32 export travels through, the engine's included). The slot is swapped for
//! [`hook`]; the PE structures come from `windows-sys` (`IMAGE_DOS_HEADER`, `IMAGE_NT_HEADERS64`,
//! `IMAGE_IMPORT_DESCRIPTOR`, `IMAGE_THUNK_DATA64`), so no offset in this file is hand-written.
//!
//! [`hook`] is one-shot: for the first call whose path is the engine's `...\csgo\bin\win64\server.dll`
//! it restores the slot(s) *first* and then loads Metamod's loader with the caller's own `hFile`
//! and flags, logging `[cs2mcp-mmload] redirect: <from> -> <to>`. Every other call - and every call
//! after that first one, including Metamod's loader's own load of the real game image - is
//! forwarded untouched.
//!
//! # Fail-closed
//!
//! `CS2MCP_MM_LOADER` missing/relative/the file gone, kernel32 or kernelbase not loaded, no
//! `LoadLibraryExW` export, no matching IAT slot or a slot that will not take the write: all of
//! them log one `[cs2mcp-mmload] FAIL: <reason>` line and return `FALSE` from `DllMain`. The
//! launcher's `LoadLibraryW` then returns NULL, the launcher terminates the suspended `cs2.exe` and
//! exits 3 - a boot that never starts, never a silent no-Metamod boot.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{GetLastError, SetLastError, HANDLE, HMODULE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS64, IMAGE_NT_OPTIONAL_HDR64_MAGIC,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE,
    LOAD_LIBRARY_AS_IMAGE_RESOURCE,
};
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_READWRITE};
use windows_sys::Win32::System::SystemServices::{
    IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_IMPORT_DESCRIPTOR, IMAGE_NT_SIGNATURE,
    DLL_PROCESS_ATTACH,
};
use windows_sys::Win32::System::WindowsProgramming::IMAGE_THUNK_DATA64;

use crate::{log, paths};

/// `LoadLibraryExW` as an IAT slot holds it.
type LoadLibraryExW =
    unsafe extern "system" fn(name: *const u16, file: HANDLE, flags: u32) -> HMODULE;

/// kernel32 imports the name once, so one slot is the expectation; the array is fixed (no
/// allocation in `DllMain`) and patching every match is what "hook the import" means if a Wine
/// build ever imports it twice.
const MAX_SLOTS: usize = 8;

/// `kernelbase!LoadLibraryExW`, the value a slot must hold to be ours.
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);

/// The slots we patched, so the one-shot can put them all back.
static SLOTS: [AtomicUsize; MAX_SLOTS] = [const { AtomicUsize::new(0) }; MAX_SLOTS];

/// How many entries of [`SLOTS`] are valid.
static SLOT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Set by the first redirect, before the loader is called.
static REDIRECTED: AtomicBool = AtomicBool::new(false);

/// Metamod's loader, UTF-16, NUL-terminated, from `CS2MCP_MM_LOADER` at install time.
static METAMOD: OnceLock<Vec<u16>> = OnceLock::new();

/// The DLL's entry point. Returns `FALSE` (0) on any install failure, which is what makes the
/// launcher's `LoadLibraryW` fail instead of resuming a Metamod-less cs2.exe.
#[no_mangle]
pub extern "system" fn DllMain(_module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // SAFETY: every pointer install() reads is inside a loaded image or an environment string,
        // as documented on the functions below.
        return match unsafe { install() } {
            Ok(()) => 1,
            Err(reason) => {
                log::fail(log::LOADER, &reason);
                0
            }
        };
    }
    1
}

/// Read `CS2MCP_MM_LOADER`, cache it, then patch kernel32's IAT.
///
/// # Safety
/// Must be called from `DllMain(DLL_PROCESS_ATTACH)` on the process's first thread: the IAT of a
/// loaded image is written here, which is only safe while no other thread can execute.
unsafe fn install() -> Result<(), String> {
    // The launcher owns this path (DESIGN §1): the DLL never derives it, so there is exactly one
    // place a layout change has to be made, and the launcher can refuse before cs2.exe even starts.
    let loader = std::env::var_os("CS2MCP_MM_LOADER")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "CS2MCP_MM_LOADER is not set (the launcher must pass Metamod's loader path)".to_string()
        })?;
    let loader = PathBuf::from(loader);
    if !loader.is_absolute() {
        return Err(format!(
            "CS2MCP_MM_LOADER is not an absolute Windows path: {}",
            loader.display()
        ));
    }
    if !loader.is_file() {
        return Err(format!("metamod loader missing: {}", loader.display()));
    }

    let mut metamod: Vec<u16> = loader.to_string_lossy().encode_utf16().collect();
    metamod.push(0);
    // Before the hook exists: a redirect can never find the cache empty.
    METAMOD
        .set(metamod)
        .map_err(|_| "install() ran twice".to_string())?;

    let kernel32 = module_handle("kernel32.dll")?;
    let kernelbase = module_handle("kernelbase.dll")?;
    // SAFETY: both handles are the loader's own module handles; the name is a NUL-terminated
    // literal, and a failed lookup is a normal `None`.
    let original = unsafe { GetProcAddress(kernelbase, c"LoadLibraryExW".as_ptr().cast()) }
        .ok_or_else(|| "kernelbase.dll does not export LoadLibraryExW".to_string())?
        as usize;
    ORIGINAL.store(original, Ordering::SeqCst);

    // SAFETY: kernel32 is a fully loaded PE image; see `hook_import_slots`.
    let patched = unsafe { hook_import_slots(kernel32, original)? };
    log::line(
        log::LOADER,
        &format!(
            "hooked {patched} kernel32 import slot(s) of LoadLibraryExW; metamod loader {}",
            loader.display()
        ),
    );
    Ok(())
}

/// `GetModuleHandleW` for a NUL-terminated ASCII name, as a `Result` so the caller can fail closed.
fn module_handle(name: &str) -> Result<HMODULE, String> {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    wide.push(0);
    // SAFETY: the name is a NUL-terminated UTF-16 string; the call neither loads nor locks.
    let handle = unsafe { GetModuleHandleW(wide.as_ptr()) };
    if handle.is_null() {
        return Err(format!("{name} is not loaded"));
    }
    Ok(handle)
}

/// Patch every IAT slot of `module` that currently holds `original`; returns how many.
///
/// # Safety
/// `module` must be a loaded PE image and no other thread may be executing image code (the
/// `DllMain(PROCESS_ATTACH)` case). The walk reads the image's own headers; every pointer is
/// derived from them, exactly as the Windows loader does it.
unsafe fn hook_import_slots(module: HMODULE, original: usize) -> Result<usize, String> {
    let base = module as *const u8;
    // SAFETY: a loaded image starts with its DOS header. The header is packed, so its fields are
    // read as values through the pointer (`(*dos).field`), never through a reference to the struct:
    // a `&IMAGE_DOS_HEADER` would make every field access a misaligned reference (E0793).
    let dos = base as *const IMAGE_DOS_HEADER;
    let e_magic = unsafe { (*dos).e_magic };
    if e_magic != IMAGE_DOS_SIGNATURE {
        return Err(format!("kernel32 has no DOS header (e_magic {e_magic:#x})"));
    }
    let e_lfanew = unsafe { (*dos).e_lfanew };
    if e_lfanew < 0 || e_lfanew % 4 != 0 {
        return Err(format!("kernel32 has an unusable e_lfanew ({e_lfanew})"));
    }
    // SAFETY: `e_lfanew` is the offset of the NT headers, per the PE/COFF spec, and is 4-aligned
    // (checked above), which is all windows-sys' `IMAGE_NT_HEADERS64` needs (its optional header
    // is `packed(4)`); the image base itself is page-aligned.
    let nt = unsafe { &*(base.add(e_lfanew as usize) as *const IMAGE_NT_HEADERS64) };
    if nt.Signature != IMAGE_NT_SIGNATURE {
        return Err(format!(
            "kernel32 has no PE signature (Signature {:#x})",
            nt.Signature
        ));
    }
    if nt.OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC {
        return Err(format!(
            "kernel32 is not PE32+ (Magic {:#x}): this hook is 64-bit only",
            nt.OptionalHeader.Magic
        ));
    }
    let imports = nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT as usize];
    if imports.VirtualAddress == 0 {
        return Err("kernel32 has no import directory".to_string());
    }

    let mut patched = 0usize;
    // SAFETY: the import directory is a NUL-terminated array of descriptors inside the image; the
    // `Name == 0` terminator is the PE/COFF rule the loader itself uses.
    let mut descriptor = unsafe { base.add(imports.VirtualAddress as usize) }
        as *const IMAGE_IMPORT_DESCRIPTOR;
    loop {
        // SAFETY: `descriptor` is inside the image (walked from the directory's start).
        let entry = unsafe { &*descriptor };
        if entry.Name == 0 {
            break;
        }
        if entry.FirstThunk != 0 {
            // SAFETY: the IAT of this descriptor is a NUL-terminated array of 8-byte thunks.
            let mut thunk = unsafe { base.add(entry.FirstThunk as usize) } as *mut IMAGE_THUNK_DATA64;
            loop {
                // SAFETY: as above; a union field read of an initialised thunk.
                let value = unsafe { (*thunk).u1.Function } as usize;
                if value == 0 {
                    break;
                }
                if value == original {
                    let failure = if patched == MAX_SLOTS {
                        Some(format!(
                            "kernel32 imports LoadLibraryExW in more than {MAX_SLOTS} slots"
                        ))
                    } else {
                        // SAFETY: the slot is inside the image and holds `original`; the write
                        // goes through the page protection dance in `write_thunk`.
                        unsafe { write_thunk(thunk, hook as *const () as usize) }.err()
                    };
                    if let Some(reason) = failure {
                        // DllMain is about to return FALSE and the DLL will be unmapped: no slot
                        // may keep pointing into it.
                        SLOT_COUNT.store(patched, Ordering::SeqCst);
                        // SAFETY: the first `patched` slots are recorded and hold our hook.
                        unsafe { restore_import_slots() };
                        return Err(reason);
                    }
                    SLOTS[patched].store(thunk as usize, Ordering::SeqCst);
                    patched += 1;
                }
                // SAFETY: advancing within the NUL-terminated IAT.
                thunk = unsafe { thunk.add(1) };
            }
        }
        // SAFETY: advancing within the NUL-terminated descriptor array.
        descriptor = unsafe { descriptor.add(1) };
    }

    if patched == 0 {
        return Err(
            "kernel32 IAT has no slot holding kernelbase!LoadLibraryExW (Wine/Windows layout changed?)"
                .to_string(),
        );
    }
    SLOT_COUNT.store(patched, Ordering::SeqCst);
    Ok(patched)
}

/// Write `value` into an IAT slot, read-only page or not.
///
/// # Safety
/// `thunk` must point at a live import thunk inside a loaded image.
unsafe fn write_thunk(thunk: *mut IMAGE_THUNK_DATA64, value: usize) -> Result<(), String> {
    let size = core::mem::size_of::<IMAGE_THUNK_DATA64>();
    let mut old = 0u32;
    // SAFETY: the slot is a mapped, image-backed page; VirtualProtect only changes its protection.
    if unsafe { VirtualProtect(thunk as *const c_void, size, PAGE_READWRITE, &mut old) } == 0 {
        return Err(format!(
            "VirtualProtect(IAT slot) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut replacement = IMAGE_THUNK_DATA64::default();
    replacement.u1.Function = value as u64;
    // SAFETY: the slot is writable now; volatile so the write cannot be elided (another thread may
    // read the slot through the loader, which the compiler cannot see).
    unsafe { thunk.write_volatile(replacement) };
    let mut ignored = 0u32;
    // SAFETY: restoring the original protection; a failure here leaves the page writable, which is
    // harmless for a slot we only point at our own function.
    unsafe { VirtualProtect(thunk as *const c_void, size, old, &mut ignored) };
    Ok(())
}

/// Put every patched slot back to `kernelbase!LoadLibraryExW`. Best effort: the one-shot must not
/// fail the redirect over a page-protection error, and the hook stays correct (the hook forwards
/// through [`ORIGINAL`] either way).
///
/// # Safety
/// Must run before [`ORIGINAL`] is used to call the real function from [`hook`]; the slots are the
/// ones [`hook_import_slots`] recorded.
unsafe fn restore_import_slots() {
    let count = SLOT_COUNT.load(Ordering::SeqCst).min(MAX_SLOTS);
    for slot in SLOTS.iter().take(count) {
        let address = slot.load(Ordering::SeqCst);
        if address != 0 {
            // SAFETY: recorded by `hook_import_slots` as a live IAT thunk.
            let _ = unsafe {
                write_thunk(
                    address as *mut IMAGE_THUNK_DATA64,
                    ORIGINAL.load(Ordering::SeqCst),
                )
            };
        }
    }
}

/// The hook itself: the engine's `server.dll` request goes to Metamod, everything else passes.
///
/// # Safety
/// Called through kernel32's import slot, so it must keep `LoadLibraryExW`'s exact `extern
/// "system"` ABI and forwarding contract.
unsafe extern "system" fn hook(name: *const u16, file: HANDLE, flags: u32) -> HMODULE {
    // SAFETY: `install` stored kernelbase!LoadLibraryExW before any slot could reach this function.
    let original: LoadLibraryExW = unsafe { core::mem::transmute(ORIGINAL.load(Ordering::SeqCst)) };

    if !name.is_null() {
        // SAFETY: `name` is the caller's NUL-terminated UTF-16 path, the `LoadLibraryExW`
        // contract.
        let path = unsafe { wide_to_string(name) };
        // A data/resource mapping (a version-resource read, say) is not the engine loading its
        // game DLL: it must neither get Metamod's image nor spend the one shot.
        let probe = flags
            & (LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE | LOAD_LIBRARY_AS_IMAGE_RESOURCE)
            != 0;
        if !probe && paths::is_windows_server_dll(&path) && !REDIRECTED.swap(true, Ordering::SeqCst) {
            // One shot: restore first, so everything Metamod's loader does - including its own load
            // of the real game image, and any later load by the engine - reaches the real function.
            // SAFETY: see the function docs; the slots were recorded at install time.
            unsafe { restore_import_slots() };
            let loader = METAMOD.get().expect("install() ran before any slot was patched");
            let target = String::from_utf16_lossy(&loader[..loader.len() - 1]);
            log::line(log::LOADER, &format!("redirect: {path} -> {target}"));
            // SAFETY: `original` is kernelbase!LoadLibraryExW; `loader` is a NUL-terminated UTF-16
            // path and the caller's own `hFile`/flags are forwarded unchanged.
            let module = unsafe { original(loader.as_ptr(), file, flags) };
            if module.is_null() {
                // The engine gets its NULL (and its own failure path); the reason is on our line.
                // Logging overwrites the thread's last error, which the engine reads next: keep it.
                // SAFETY: plain thread-local error-code accessors.
                let code = unsafe { GetLastError() };
                log::fail(
                    log::LOADER,
                    &format!(
                        "metamod loader did not load: {}",
                        std::io::Error::from_raw_os_error(code as i32)
                    ),
                );
                unsafe { SetLastError(code) };
            }
            return module;
        }
    }

    // SAFETY: pass the caller's arguments through unchanged.
    unsafe { original(name, file, flags) }
}

/// A NUL-terminated UTF-16 string as a `String` (lossy: a path is what it is, and the match below
/// is ASCII).
///
/// # Safety
/// `p` must be NUL-terminated.
unsafe fn wide_to_string(p: *const u16) -> String {
    let mut len = 0usize;
    // SAFETY: the caller guarantees a terminator.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units are initialised and precede the terminator.
    String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(p, len) })
}
