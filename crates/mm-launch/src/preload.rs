//! Linux: the `LD_PRELOAD` `dlopen` interposer.
//!
//! One exported symbol - `dlopen` - so every `dlopen` in the process comes through here first. The
//! real function is `dlsym(RTLD_NEXT, "dlopen")`, resolved once and cached; everything is passed
//! through untouched except the engine's first request for its own
//! `.../csgo/bin/linuxsteamrt64/libserver.so`, which is answered with Metamod's loader at
//! `.../csgo/addons/metamod/bin/linuxsteamrt64/libserver.so` and the caller's own flags.
//!
//! One shot, spent before the loader is called: Metamod's loader immediately re-`dlopen`s the
//! engine's image itself, live as `.../game/bin/linuxsteamrt64/../../csgo/bin/linuxsteamrt64//libserver.so`
//! (note the `//`, which the suffix match does not accept either), and that request must reach the
//! real function. The one shot is the guarantee; the `//` is luck - do not normalise it away and
//! drop the one shot. Fail-closed (user decision Q2): Metamod's files missing at the redirect
//! is `[cs2mcp-mmload] FAIL: ...` on stderr plus `abort()` - the process must not come up without
//! Metamod on a path where the launcher promised it.
//!
//! Not compiled into test builds: see the note in `lib.rs`.

use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::ffi::{CStr, CString};
use std::path::Path;

use crate::{log, paths};

/// `dlopen` as the dynamic loader exports it.
type Dlopen = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;

/// The real `dlopen`, or 0 before the first call. `usize` so it can live in a static without a
/// `Sync` wrapper; the cast back to [`Dlopen`] happens per call.
static REAL: AtomicUsize = AtomicUsize::new(0);

/// Set by the first redirect, before the loader is called: one shot, so Metamod's own load of the
/// real game image (and any later request) reaches the real `dlopen`.
static REDIRECTED: AtomicBool = AtomicBool::new(false);

/// Resolve and cache `dlsym(RTLD_NEXT, "dlopen")`.
///
/// # Safety
/// Called from our `dlopen`, before the real one runs, so no loader lock is held yet; an
/// `RTLD_NEXT` lookup from inside a preloaded object is the supported way to find the next
/// definition.
unsafe fn real_dlopen() -> Dlopen {
    let mut address = REAL.load(Ordering::Acquire);
    if address == 0 {
        // SAFETY: `RTLD_NEXT` is the supported way to find the next definition of a symbol from
        // inside a preloaded object; the name is a NUL-terminated literal.
        address = unsafe { libc::dlsym(libc::RTLD_NEXT, c"dlopen".as_ptr()) } as usize;
        if address == 0 {
            // Nothing to forward to. Silence here would turn every `dlopen` in the process into a
            // NULL return, which is worse than a loud stop.
            log::fail(log::LOADER, "dlsym(RTLD_NEXT, dlopen) failed");
            // SAFETY: abort() has no preconditions and does not return.
            unsafe { libc::abort() }
        }
        REAL.store(address, Ordering::Release);
    }
    // SAFETY: the address came from dlsym for the symbol name `dlopen`, so it is that function.
    unsafe { core::mem::transmute::<usize, Dlopen>(address) }
}

/// The interposed entry point.
///
/// # Safety
/// Same contract as `dlopen(3)`: `filename` is NULL or a NUL-terminated string, and the flags are
/// the caller's `RTLD_*` set. The returned handle is the loader's, or NULL with `dlerror` set.
#[no_mangle]
pub unsafe extern "C" fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void {
    // SAFETY: see the function contract.
    let real = unsafe { real_dlopen() };

    // SAFETY: `filename` is NULL or NUL-terminated by the caller's contract.
    let name = if filename.is_null() {
        None
    } else {
        // A non-UTF-8 path cannot be the engine's image (those are ASCII); anything else is the
        // caller's business and passes through.
        unsafe { CStr::from_ptr(filename) }.to_str().ok()
    };

    if let Some(name) = name {
        // An RTLD_NOLOAD probe ("is it mapped yet?") is not the engine loading its game image: it
        // must neither get Metamod's handle nor spend the one shot.
        let probe = flags & libc::RTLD_NOLOAD != 0;
        if !probe && paths::is_linux_server_so(name) && !REDIRECTED.swap(true, Ordering::SeqCst) {
            // `linux_metamod_loader` cannot say `None` for a path `is_linux_server_so` accepted.
            let target = paths::linux_metamod_loader(name).unwrap_or_default();
            if !Path::new(&target).is_file() {
                log::fail(
                    log::LOADER,
                    &format!("metamod loader missing: {target} (asked for {name}); refusing to load the game's own image"),
                );
                // SAFETY: abort() has no preconditions and does not return.
                unsafe { libc::abort() }
            }
            log::line(log::LOADER, &format!("redirect: {name} -> {target}"));
            // `name` is a C string (no interior NUL) and the suffix we append has none either.
            let target = CString::new(target).expect("no interior NUL in a derived path");
            // SAFETY: both pointers are valid for the call; the flags are the caller's own.
            let handle = unsafe { real(target.as_ptr(), flags) };
            if handle.is_null() {
                // The caller still gets the NULL handle (and its own `dlerror`), but the reason is
                // on our line rather than buried in a Metamod log the engine may not have started.
                log::fail(log::LOADER, "Metamod's loader did not load");
            }
            return handle;
        }
    }

    // SAFETY: pass the caller's arguments through unchanged.
    unsafe { real(filename, flags) }
}
