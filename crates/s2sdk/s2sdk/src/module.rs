//! Already-loaded game libraries: find them, resolve symbols, call their
//! `CreateInterface` factories. Never loads anything new.
//!
//! Everything here is non-blocking by design: [`Module::find_loaded`] checks what the dynamic
//! linker has **already** mapped. The mechanism is per target:
//!
//! * **Linux** (`cfg(not(windows))`): `dl_iterate_phdr` to find the mapping whose `dlpi_name`
//!   basename matches, then `dlopen(full_path, RTLD_LAZY | RTLD_NOLOAD)` for the handle (the
//!   same method the WP7a probe verified live — the engine's own handle comes back for the
//!   SONAME lookup).
//! * **Windows** (`cfg(windows)`): `GetModuleHandleW` for the handle (it never loads, and it
//!   accepts a bare file name), then `GetModuleFileNameW` for the mapped path.
//!
//! The probe polls it on its own deadline instead of any call here waiting or sleeping.
//! Interface version strings are upstream's, in [`versions`].
//!
//! # The mapped image
//!
//! [`Module::find_loaded`] also records the mapping the loader described — the base, the mapped
//! span and the readable RVA ranges (Windows: the header block plus every `IMAGE_SCN_MEM_READ`
//! section; Linux: the `PF_R` `PT_LOAD` segments) — and [`Module::image`] hands it to
//! [`crate::rtti`] as an in-place [`ImageRange`]. It is computed once, when the module is
//! found; nothing here re-asks the loader or opens the module's file.
//!
//! # Which image a name means (Metamod loads a second `server.dll`)
//!
//! [`Module::find_game`] is the one finder for the game's own libraries. [`Module::find_loaded`]
//! answers the **first** mapping whose base name matches — and that is not the game's image once
//! Metamod is installed: Metamod maps its own loader (`server.dll` / `libserver.so`) before the
//! game's, so asking it for `CreateInterface` asks the stub (it forwards while Metamod's core is
//! up, but re-runs Metamod's load path when its gamedll bridge is null: an extra `LoadLibrary`
//! and a re-patched `Connect`). [`mapped`] enumerates every image of the name, [`select_module`]
//! applies the rules — never a `metamod` image below the game root, prefer the game root, refuse
//! when still ambiguous — and [`Module::find_game`] opens the one it chose.
//!
//! # Module file names are per target
//!
//! The Linux and Windows installs spell the same game libraries differently — Linux
//! `lib<stem>.so`, Windows `<stem>.dll`: `libschemasystem.so`/`schemasystem.dll`,
//! `libserver.so`/`server.dll`, `libtier0.so`/`tier0.dll`, `libengine2.so`/`engine2.dll`.
//! [`names`] is the single table of those spellings; every caller in this crate goes through
//! it, so no file name is hard-coded anywhere else. The Linux values are the live-verified
//! (WP6b) ones; the Windows values are the same stems with the Windows extension and are
//! proven only by the Windows type-check, not live.
//!
//! # Calling convention
//!
//! `CreateInterfaceFn` and every generated vtable entry are `extern "C"`. On
//! `x86_64-pc-windows-msvc` that *is* the Microsoft x64 ABI, which is also what MSVC uses for
//! a C++ non-static member function (the `this` pointer goes in `RCX`); there is no separate
//! `thiscall`, so the generated `extern "C" fn(this, …)` entries are correct on both targets.

use core::ffi::{c_char, c_int, c_void};
use std::ffi::CString;
use std::path::{Path, PathBuf};

use crate::rtti::{Abi, ImageRange};

// The `dl_iterate_phdr` callback reads a NUL-terminated `dlpi_name`; only that path needs it.
#[cfg(not(windows))]
use std::ffi::CStr;

/// Interface version strings, upstream's own. The macros live in
/// `public/interfaces/interfaces.h` (`SCHEMASYSTEM_INTERFACE_VERSION`,
/// `CVAR_INTERFACE_VERSION`) and `public/schemasystem/ischemasystem.h`; consult that file
/// first when an upstream bump renames one.
///
/// The version strings are target-independent (the same macro value is compiled into both
/// installs); only the module that serves them is named per target — see [`names`].
pub mod versions {
    /// `ISchemaSystem` factory name (upstream `SCHEMASYSTEM_INTERFACE_VERSION`), served by
    /// the schema system module (Linux `libschemasystem.so`, Windows `schemasystem.dll`; the
    /// Linux side is live-verified, WP6b: created ~120 ms into the process).
    pub const SCHEMASYSTEM_INTERFACE_VERSION: &str = "SchemaSystem_001";

    /// `ICvar` factory name (upstream `CVAR_INTERFACE_VERSION`). Live-verified (WP6b): the
    /// Linux dedicated server serves it from **`libtier0.so`** — see [`crate::cvar::CVAR_LIBS`].
    pub const CVAR_INTERFACE_VERSION: &str = "VEngineCvar007";

    /// `IGameEventManager2` factory name. Live-verified spelling (WP6b): the string Valve's own
    /// Linux modules carry is the **uppercase** `"GAMEEVENTSMANAGER002"` (`libclient.so` and
    /// `libmatchmaking.so` contain it; `libengine2.so`, `libserver.so` and `libtier0.so` do
    /// not). On the Linux dedicated server the name resolves to nothing — see `crate::events`
    /// for both lookup strategies.
    pub const GAME_EVENTS_MANAGER_INTERFACE_VERSION: &str = "GAMEEVENTSMANAGER002";
}

/// Per-target file names of the game's own libraries, matched by [`Module::find_loaded`].
///
/// The convention is fixed by the two installs: Linux `lib<stem>.so`, Windows `<stem>.dll`.
/// The Linux values are the live-verified ones (WP6b); the Windows values use the same stems
/// with the Windows extension and are proven only by the Windows type-check of this crate
/// (no Windows CS2 install is exercised by these tests).
pub mod names {
    /// The schema system module (`SchemaSystem_001`): `libschemasystem.so` / `schemasystem.dll`.
    #[cfg(not(windows))]
    pub const SCHEMASYSTEM: &str = "libschemasystem.so";
    /// The schema system module (`SchemaSystem_001`): `libschemasystem.so` / `schemasystem.dll`.
    #[cfg(windows)]
    pub const SCHEMASYSTEM: &str = "schemasystem.dll";

    /// The server module, whose schema type scope is keyed under this name
    /// (`FindTypeScopeForModule`): `libserver.so` / `server.dll`.
    #[cfg(not(windows))]
    pub const SERVER: &str = "libserver.so";
    /// The server module, whose schema type scope is keyed under this name
    /// (`FindTypeScopeForModule`): `libserver.so` / `server.dll`.
    #[cfg(windows)]
    pub const SERVER: &str = "server.dll";

    /// The `tier0` module (`ICvar` = `VEngineCvar007`): `libtier0.so` / `tier0.dll`.
    #[cfg(not(windows))]
    pub const TIER0: &str = "libtier0.so";
    /// The `tier0` module (`ICvar` = `VEngineCvar007`): `libtier0.so` / `tier0.dll`.
    #[cfg(windows)]
    pub const TIER0: &str = "tier0.dll";

    /// The `engine2` module (`CreateInterface`; also strategy A for `IGameEventManager2`):
    /// `libengine2.so` / `engine2.dll`.
    #[cfg(not(windows))]
    pub const ENGINE2: &str = "libengine2.so";
    /// The `engine2` module (`CreateInterface`; also strategy A for `IGameEventManager2`):
    /// `libengine2.so` / `engine2.dll`.
    #[cfg(windows)]
    pub const ENGINE2: &str = "engine2.dll";
}

// ---------------------------------------------------------------------------
// Which image a module name means (Metamod loads a second `server.dll`)
// ---------------------------------------------------------------------------

/// One image the loader has mapped under a module's file name: where it is and where it came
/// from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedModule {
    /// The load base (what `GetModuleHandle*` answers as its `HMODULE`; the ELF load bias on
    /// Linux).
    pub base: u64,
    /// The full path the loader holds for the image (`GetModuleFileNameW` on Windows,
    /// `dl_iterate_phdr`'s `dlpi_name` on Linux). Empty when an adapter cannot name it (a host
    /// test's simplest double), which the selection treats as "the only candidate".
    pub path: String,
}

/// Why an image that exists under a module's name is not the one to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Every image mapped under the name sits under a Metamod directory: that `server.dll` is
    /// Metamod's loader stub, not the game's binary (it loads the game's image later).
    LoaderStub,
    /// More than one image is a candidate (more than one under the game root): this build
    /// verifies one image and fails closed rather than guess.
    Ambiguous,
}

impl Reject {
    /// The one log line a refusal writes (once per module per process), after the module name.
    #[must_use]
    pub const fn line(self) -> &'static str {
        match self {
            Self::LoaderStub => {
                "the only image mapped under this name is Metamod's loader stub; not used"
            }
            Self::Ambiguous => "more than one game image is mapped under this name; not used",
        }
    }
}

/// The path's file name (after the last `\` or `/`).
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The path's components, split on either separator, without empty ones.
fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split(['\\', '/']).filter(|part| !part.is_empty())
}

/// Whether `path` is inside `root`, compared component by component and case-insensitively
/// (Windows path comparison ignores case; the same rule reads a Linux path harmlessly).
fn under(root: &str, path: &str) -> bool {
    let mut path_parts = components(path);
    components(root).all(|root_part| {
        path_parts
            .next()
            .is_some_and(|part| part.eq_ignore_ascii_case(root_part))
    })
}

/// Whether a `metamod` path component marks `path` as Metamod's loader stub.
///
/// Only the part **below the game root** is inspected: an install that itself lives under a
/// `metamod` directory (`D:\metamod\cs2\game\...`) must not have every one of its modules
/// refused (review N1). With no root - or a path that is not under it - there is nothing to cut
/// off, so the whole path is inspected: Metamod's stub is still the image that must never be
/// verified.
fn is_loader_stub(root: Option<&str>, path: &str) -> bool {
    let skip = match root {
        Some(root) if under(root, path) => components(root).count(),
        _ => 0,
    };
    components(path)
        .skip(skip)
        .any(|part| part.eq_ignore_ascii_case("metamod"))
}

/// The game root, derived from the host executable's path: its directory with the loader
/// directories above it removed - `<game>\bin\win64\cs2.exe` answers `<game>`, and so does
/// `<game>/bin/linuxsteamrt64/cs2` (review N3's Linux layout).
///
/// `None` when the path has no components: the selection then relies on the Metamod filter
/// alone.
#[must_use]
pub fn game_root(host: &str) -> Option<String> {
    let parts: Vec<&str> = components(host).collect();
    // The executable itself, then the loader directories above it.
    let mut dirs = parts.len().checked_sub(1)?;
    while dirs > 0 {
        let last = parts[dirs - 1];
        if last.eq_ignore_ascii_case("win64")
            || last.eq_ignore_ascii_case("linuxsteamrt64")
            || last.eq_ignore_ascii_case("linux64")
            || last.eq_ignore_ascii_case("bin")
        {
            dirs -= 1;
        } else {
            break;
        }
    }
    if dirs == 0 {
        return None;
    }
    // Keep the separator style (and any drive prefix) of the input, and the absoluteness a
    // leading separator spells (`components` drops it).
    let sep = if host.contains('\\') { '\\' } else { '/' };
    let mut root = parts[..dirs].join(&sep.to_string());
    if host.starts_with(['\\', '/']) {
        root.insert(0, sep);
    }
    Some(root)
}

/// Choose the image a module name names among the images the loader has mapped under it.
///
/// `candidates` are the images whose **file name** is `name` ([`mapped`] filters by name; a
/// [`LoadedModule::path`] of `""` skips the path rules). The rules, in order:
///
/// 1. an image whose path (below the game root, see [`is_loader_stub`]) has a `metamod`
///    component is never chosen ([`Reject::LoaderStub`] when it is all there is) - Metamod's
///    `server.dll` is a loader stub, and Windows' `GetModuleHandleA` answers the **first**
///    module whose base name matches, which is that stub whenever Metamod is installed
///    (Microsoft Learn, `GetModuleHandleA` Remarks: the name is compared case-independently to
///    the base names of the modules mapped in the process);
/// 2. among what is left, an image under the game root ([`game_root`]) is preferred - a
///    relocated install where nothing is under the root still works if exactly one image is
///    left;
/// 3. anything still ambiguous answers [`Reject::Ambiguous`]: this build verifies one image and
///    fails closed rather than pick.
///
/// `Ok(None)` means nothing is mapped under the name (the C's "not loaded").
pub fn select_module<'a>(
    name: &str,
    host_path: Option<&str>,
    candidates: &'a [LoadedModule],
) -> Result<Option<&'a LoadedModule>, Reject> {
    let named: Vec<&LoadedModule> = candidates
        .iter()
        .filter(|candidate| {
            candidate.path.is_empty() || file_name(&candidate.path).eq_ignore_ascii_case(name)
        })
        .collect();
    if named.is_empty() {
        return Ok(None);
    }
    let root = host_path.and_then(game_root);
    let mut usable: Vec<&LoadedModule> = named
        .into_iter()
        .filter(|candidate| {
            candidate.path.is_empty() || !is_loader_stub(root.as_deref(), &candidate.path)
        })
        .collect();
    if usable.is_empty() {
        return Err(Reject::LoaderStub);
    }
    if let Some(root) = root.as_deref() {
        let in_game: Vec<&LoadedModule> = usable
            .iter()
            .copied()
            .filter(|candidate| candidate.path.is_empty() || under(root, &candidate.path))
            .collect();
        if !in_game.is_empty() {
            usable = in_game;
        }
    }
    match usable.as_slice() {
        [only] => Ok(Some(only)),
        _ => Err(Reject::Ambiguous),
    }
}

/// Every image the loader has mapped whose file name is `name` (case-insensitive), in loader
/// order, with the path it was loaded from.
///
/// This is the one enumeration behind the game-module finder: [`select_module`] applies the
/// rules to its answer and [`Module::find_game`] opens the image it chose. On Windows it is
/// `K32EnumProcessModules` + `GetModuleFileNameW` (the whole process, so Metamod's second
/// `server.dll` is visible); on Linux every `dl_iterate_phdr` match by base name (where
/// Metamod's loader is also `libserver.so`).
///
/// A process with more than sixteen images of one name (Linux) is not a game install: the walk
/// stops there and the selection sees only what was collected.
#[must_use]
pub fn mapped(name: &str) -> Vec<LoadedModule> {
    platform::mapped(name)
}

/// The host executable's full path (`GetModuleFileNameW(NULL)` on Windows, `/proc/self/exe` on
/// Linux): the anchor [`game_root`] is derived from.
#[must_use]
pub fn host_executable() -> Option<String> {
    platform::host_executable()
}

/// The loader queries behind [`mapped`] / [`host_executable`], split by target.
#[cfg(windows)]
mod platform {
    use super::{file_name, LoadedModule};
    use core::ffi::c_void;

    use windows_sys::Win32::Foundation::{HANDLE, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows_sys::Win32::System::ProcessStatus::K32EnumProcessModules;

    /// `GetCurrentProcess()`'s pseudo-handle (`processthreadsapi.h`: the function returns
    /// `(HANDLE)-1`). Spelled as the documented constant because `GetCurrentProcess` lives in
    /// the `Win32_System_Threading` feature this crate does not enable, and
    /// `K32EnumProcessModules` needs only the handle.
    const CURRENT_PROCESS: HANDLE = -1isize as HANDLE;

    /// A module's full path (`GetModuleFileNameW`), or `None` when the loader cannot name it.
    pub(super) fn module_path(handle: HMODULE) -> Option<String> {
        let mut buf = vec![0u16; 260]; // MAX_PATH
        loop {
            // SAFETY: `handle` is a live module handle (or null for the host executable) and
            // `buf` is writable for its length.
            let len = unsafe { GetModuleFileNameW(handle, buf.as_mut_ptr(), buf.len() as u32) };
            let len = usize::try_from(len).ok()?;
            if len == 0 {
                return None;
            }
            if len < buf.len() {
                buf.truncate(len);
                return Some(String::from_utf16_lossy(&buf));
            }
            if buf.len() >= 32_768 {
                return None; // beyond any real path length; do not loop forever
            }
            buf.resize(buf.len() * 2, 0);
        }
    }

    /// See [`super::mapped`].
    pub(super) fn mapped(name: &str) -> Vec<LoadedModule> {
        let mut capacity = 64usize;
        loop {
            let mut modules = vec![core::ptr::null_mut::<c_void>() as HMODULE; capacity];
            let mut needed = 0u32;
            let bytes =
                u32::try_from(capacity * core::mem::size_of::<HMODULE>()).unwrap_or(u32::MAX);
            // SAFETY: `modules` is writable for `capacity` handles and `needed` for a u32; the
            // call reads the process handle only.
            let ok = unsafe {
                K32EnumProcessModules(CURRENT_PROCESS, modules.as_mut_ptr(), bytes, &mut needed)
            };
            let needed = usize::try_from(needed).unwrap_or(0);
            if needed > usize::try_from(bytes).unwrap_or(usize::MAX) {
                // The buffer was too small (or the list grew while it was written): the call
                // answered the size it needs - ask again with room for it.
                capacity = needed / core::mem::size_of::<HMODULE>() + 16;
                if capacity > 65_536 {
                    return Vec::new();
                }
                continue;
            }
            if ok == 0 {
                return Vec::new();
            }
            let count = needed / core::mem::size_of::<HMODULE>();
            return modules[..count]
                .iter()
                .filter_map(|&handle| {
                    let path = module_path(handle)?;
                    if file_name(&path).eq_ignore_ascii_case(name) {
                        Some(LoadedModule {
                            base: handle as usize as u64,
                            path,
                        })
                    } else {
                        None
                    }
                })
                .collect();
        }
    }

    /// See [`super::host_executable`].
    pub(super) fn host_executable() -> Option<String> {
        module_path(core::ptr::null_mut())
    }
}

/// The loader queries behind [`mapped`] / [`host_executable`], split by target.
#[cfg(not(windows))]
mod platform {
    use super::LoadedModule;

    /// The most images of one base name the Linux walk describes. A real CS2 process maps two
    /// (`libserver.so` and Metamod's loader stub); a process with more of one name is not a game
    /// install.
    pub(super) const MAX_PHDR_MATCHES: usize = 16;

    /// One match the walk collected: the load bias, the path bytes and their length.
    type Hit = (u64, [u8; super::PHDR_PATH_MAX], usize);

    /// Result carrier for the collecting `dl_iterate_phdr` callback: the base name to match and
    /// the matches in loader order. Fixed storage, so the callback never allocates (see the
    /// repo rule: the callback must not panic; the `Vec` is built after the iteration).
    #[repr(C)]
    pub(super) struct PhdrMatches<'a> {
        pub(super) file_name: &'a [u8],
        pub(super) count: usize,
        pub(super) hits: [Hit; MAX_PHDR_MATCHES],
    }

    /// The collecting callback: every mapping whose basename matches is recorded (the loader
    /// calls this for each object in load order); never panics - no allocation, no indexing
    /// panic (the slot is checked against the fixed length).
    pub(super) unsafe extern "C" fn matches_callback(
        info: *mut libc::dl_phdr_info,
        _size: usize,
        data: *mut core::ffi::c_void,
    ) -> i32 {
        use std::ffi::CStr;

        // SAFETY: glibc passes a valid `dl_phdr_info` and our data pointer for the iteration.
        if info.is_null() || data.is_null() {
            return 0;
        }
        let walk = unsafe { &mut *(data as *mut PhdrMatches<'_>) };
        if walk.count == MAX_PHDR_MATCHES {
            return 1; // the fixed storage is full: stop
        }
        // SAFETY: `dlpi_name` is a NUL-terminated string owned by the loader for the iteration.
        let name = unsafe { (*info).dlpi_name };
        if name.is_null() {
            return 0;
        }
        let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
        if !super::basename_matches(bytes, walk.file_name) || bytes.len() >= super::PHDR_PATH_MAX {
            return 0;
        }
        let slot = walk.count;
        // SAFETY: `dlpi_addr` is this object's load bias.
        walk.hits[slot].0 = unsafe { (*info).dlpi_addr };
        walk.hits[slot].1[..bytes.len()].copy_from_slice(bytes);
        walk.hits[slot].2 = bytes.len();
        walk.count += 1;
        0 // keep walking: every match matters
    }

    /// See [`super::mapped`].
    pub(super) fn mapped(name: &str) -> Vec<LoadedModule> {
        let mut walk = PhdrMatches {
            file_name: name.as_bytes(),
            count: 0,
            hits: [(0, [0; super::PHDR_PATH_MAX], 0); MAX_PHDR_MATCHES],
        };
        // SAFETY: the walk struct outlives the call and the callback only touches it.
        unsafe {
            libc::dl_iterate_phdr(
                Some(matches_callback),
                core::ptr::from_mut(&mut walk).cast::<core::ffi::c_void>(),
            );
        }
        walk.hits[..walk.count]
            .iter()
            .map(|(base, path, len)| LoadedModule {
                base: *base,
                // The loader's path is a byte string; a non-UTF-8 one can only make the open
                // fail later, which fails closed (`Module::find_game` answers `None`).
                path: String::from_utf8_lossy(&path[..*len]).into_owned(),
            })
            .collect()
    }

    /// See [`super::host_executable`]: the executable the kernel is running, resolved.
    pub(super) fn host_executable() -> Option<String> {
        std::fs::read_link("/proc/self/exe")
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    }
}

/// A library already mapped in this process.
///
/// **Linux:** the handle comes from `dlopen(full_path, RTLD_LAZY | RTLD_NOLOAD)`: a `NOLOAD`
/// lookup only bumps the reference count on a mapping that exists, never loads anything, so
/// dropping the [`Module`] (`dlclose`) is safe and leaves the game's own mapping untouched.
/// `RTLD_LAZY` (not `RTLD_NOW`) keeps the lookup working on a mapped library whose PLT entries
/// are not all resolved yet — `RTLD_NOW` would fail exactly on a lib the game mapped lazily.
///
/// **Windows:** the handle comes from `GetModuleHandleW`, which never loads and carries no
/// reference to release, so dropping the [`Module`] does nothing (there is no Windows
/// [`Drop`] impl).
///
/// The [`image`](Module::image) is worked out here, once, from the same loader query that
/// found the module: the base, the mapped span and the readable RVA ranges. Nothing else in the
/// crate asks the loader again, and no file is opened.
#[derive(Debug)]
pub struct Module {
    handle: *mut c_void,
    path: PathBuf,
    image: ImageRange<'static>,
}

// SAFETY: the platform handles are process-global and immutable here; `dlsym` on Linux and
// `GetProcAddress` on Windows are thread-safe. The library's *contents* are not synchronized
// by this wrapper, but nothing in it reads library data concurrently — it only resolves
// symbols.
unsafe impl Send for Module {}
// SAFETY: same as `Send` — the handle and path are never mutated; both symbol lookups are
// thread-safe.
unsafe impl Sync for Module {}

/// Fixed buffer the `dl_iterate_phdr` callback fills: no allocation, therefore no panic path
/// inside the callback (see the repo rule: the callback must not panic; the search happens
/// after the iteration).
#[cfg(not(windows))]
const PHDR_PATH_MAX: usize = 1024;

/// The `PT_LOAD` segments one module can have before the adapter refuses to describe it: real
/// modules have fewer than ten.
#[cfg(not(windows))]
const MAX_LOAD_SEGMENTS: usize = 32;

/// Result carrier for the `dl_iterate_phdr` callback: the matched full `dlpi_name` bytes
/// (truncated to [`PHDR_PATH_MAX`]), `len == 0` meaning "no match (yet)", plus the mapping the
/// image needs - the load bias, the span and the readable (`PF_R`) `PT_LOAD` segments.
#[cfg(not(windows))]
#[repr(C)]
struct PhdrProbe<'a> {
    file_name: &'a [u8],
    /// Match the whole `dlpi_name` (a path) instead of its basename.
    match_full: bool,
    hit_len: usize,
    hit: [u8; PHDR_PATH_MAX],
    base: u64,
    size: u64,
    segments: [(u64, u64); MAX_LOAD_SEGMENTS],
    count: usize,
    overflow: bool,
}

#[cfg(not(windows))]
impl PhdrProbe<'_> {
    fn matched(&self) -> Option<&[u8]> {
        if self.hit_len == 0 {
            None
        } else {
            Some(&self.hit[..self.hit_len])
        }
    }
}

/// Does `path_bytes` end with the path component `name` (basename match)?
#[cfg(not(windows))]
fn basename_matches(path_bytes: &[u8], name: &[u8]) -> bool {
    let start = path_bytes
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |pos| pos + 1);
    &path_bytes[start..] == name
}

/// The `dl_iterate_phdr` callback: match the basename of `dlpi_name`, copy the full path into
/// the probe buffer and record the load bias plus the readable `PT_LOAD` segments, stop the
/// iteration. Never panics: no allocation, no indexing panics (all slices are bounds-checked to
/// fixed sizes).
#[cfg(not(windows))]
unsafe extern "C" fn phdr_callback(
    info: *mut libc::dl_phdr_info,
    _size: usize,
    data: *mut c_void,
) -> c_int {
    // SAFETY: glibc passes a valid `dl_phdr_info` and our data pointer for the duration of the
    // iteration; we only read what it points at.
    if info.is_null() || data.is_null() {
        return 0;
    }
    let probe = unsafe { &mut *(data as *mut PhdrProbe<'_>) };
    if probe.matched().is_some() {
        return 1; // already found: stop
    }
    let name = unsafe { (*info).dlpi_name };
    if name.is_null() {
        return 0;
    }
    // SAFETY: `dlpi_name` is a NUL-terminated string owned by the loader for the iteration.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    let matched = if probe.match_full {
        bytes == probe.file_name
    } else {
        basename_matches(bytes, probe.file_name)
    };
    if !matched {
        return 0;
    }
    if bytes.len() >= PHDR_PATH_MAX {
        // Path longer than the buffer: cannot record it, keep iterating (and never match).
        return 0;
    }
    probe.hit[..bytes.len()].copy_from_slice(bytes);
    probe.hit_len = bytes.len();
    // SAFETY: `dlpi_addr` is this object's load bias and `dlpi_phdr` is `dlpi_phnum`
    // program headers the loader owns for the iteration.
    probe.base = unsafe { (*info).dlpi_addr };
    // SAFETY: `dlpi_phdr` points at `dlpi_phnum` program headers (index bounds below).
    for index in 0..usize::from(unsafe { (*info).dlpi_phnum }) {
        // SAFETY: index < dlpi_phnum.
        let header = unsafe { &*phdrs_at((*info).dlpi_phdr, index) };
        // `p_type` is `Elf64_Word` (u32); `libc::PT_LOAD` is the same C constant.
        if header.p_type != libc::PT_LOAD {
            continue;
        }
        let end = header.p_vaddr.saturating_add(header.p_memsz);
        probe.size = probe.size.max(end);
        // Only segments the loader mapped readable are recorded: the reader may read those and
        // nothing else.
        if header.p_flags & libc::PF_R == 0 {
            continue;
        }
        if probe.count == MAX_LOAD_SEGMENTS {
            probe.overflow = true;
            return 1;
        }
        probe.segments[probe.count] = (header.p_vaddr, header.p_memsz);
        probe.count += 1;
    }
    1 // found: stop
}

/// # Safety
///
/// `phdrs` must point at at least `index + 1` program headers.
#[cfg(not(windows))]
unsafe fn phdrs_at(phdrs: *const libc::Elf64_Phdr, index: usize) -> *const libc::Elf64_Phdr {
    // SAFETY: the caller guarantees the element exists.
    unsafe { phdrs.add(index) }
}

#[cfg(not(windows))]
impl Module {
    /// Find a lib already mapped in this process by file name (e.g.
    /// `"libschemasystem.so"`, or [`names`]): walk the link map with `dl_iterate_phdr`
    /// (matching the basename of `dlpi_name`), then `dlopen(full_path, RTLD_LAZY |
    /// RTLD_NOLOAD)` for a handle. `None` when the library is not mapped (and only then — a
    /// `NOLOAD` handle never loads).
    ///
    /// The same walk records the [`image`](Module::image): the load bias, the span of the
    /// `PT_LOAD` segments and the `PF_R` ones among them.
    ///
    /// Non-blocking: one link-map walk plus one `dlopen` call, no retries, no sleeps — the
    /// caller polls on its own deadline.
    ///
    /// **The first basename match wins**, which is why [`Module::find_game`] (not this) is the
    /// way to ask for a game library: Metamod's loader is also `libserver.so`, and it is
    /// mapped first.
    ///
    /// Not race-free against concurrent `dlopen`/`dlclose` by the engine (glibc's own
    /// limitation, documented for `dl_iterate_phdr`); a single-threaded poll loop is fine.
    #[must_use]
    pub fn find_loaded(file_name: &str) -> Option<Module> {
        open_probed(&probe_phdr(file_name.as_bytes(), false))
    }

    /// Find the **game's own** image mapped under `file_name` (e.g. `names::SERVER`): the one
    /// finder for a pinned module, over every mapping the loader has of that name
    /// ([`mapped`]) and the rules in [`select_module`].
    ///
    /// This is the answer to "which `libserver.so` is the game's": Metamod loads a loader stub
    /// of the same name, and `find_loaded`'s first match is the stub. `Ok(None)` means nothing
    /// is mapped under the name; a [`Reject`] means something is, but not the game's image (or
    /// not unambiguously), and the caller must fail closed.
    ///
    /// Non-blocking: one link-map walk, one `dlopen(RTLD_NOLOAD)`.
    pub fn find_game(file_name: &str) -> Result<Option<Module>, Reject> {
        let candidates = mapped(file_name);
        let host = host_executable();
        let Some(chosen) = select_module(file_name, host.as_deref(), &candidates)? else {
            return Ok(None);
        };
        // The chosen path is opened exactly: `mapped` collected the same `dlpi_name` bytes, so
        // the second walk (with the image's segments) meets the same object.
        Ok(open_probed(&probe_phdr(chosen.path.as_bytes(), true)))
    }
}

/// One `dl_iterate_phdr` walk for the mapping whose basename (or whole `dlpi_name`, with
/// `match_full`) is `file_name`, with its load bias, span and readable `PT_LOAD` segments.
#[cfg(not(windows))]
fn probe_phdr(file_name: &[u8], match_full: bool) -> PhdrProbe<'_> {
    let mut probe = PhdrProbe {
        file_name,
        match_full,
        hit_len: 0,
        hit: [0; PHDR_PATH_MAX],
        base: 0,
        size: 0,
        segments: [(0, 0); MAX_LOAD_SEGMENTS],
        count: 0,
        overflow: false,
    };
    // SAFETY: the probe struct outlives the call and the callback only touches it.
    unsafe {
        libc::dl_iterate_phdr(
            Some(phdr_callback),
            core::ptr::from_mut(&mut probe).cast::<c_void>(),
        );
    }
    probe
}

/// The [`Module`] for a probed mapping: `dlopen(full_path, RTLD_LAZY | RTLD_NOLOAD)` for the
/// handle (never loads) plus the [`ImageRange`] the walk described.
#[cfg(not(windows))]
fn open_probed(probe: &PhdrProbe<'_>) -> Option<Module> {
    use std::os::unix::ffi::OsStringExt;

    let path_bytes = probe.matched()?.to_vec();
    let cpath = CString::new(path_bytes.clone()).ok()?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(path_bytes));
    // SAFETY: `cpath` is a valid NUL-terminated string for the duration of the call.
    // `RTLD_LAZY | RTLD_NOLOAD`: `LAZY` (not `NOW`) so a mapped-but-not-yet-relocated
    // library still resolves (RTLD_NOW fails on a mapped lib with unresolved PLT
    // entries); `NOLOAD` means: return the engine's handle only if already mapped (no
    // load, live-verified in WP7a: the SONAME lookup returns the engine's handle).
    let handle = unsafe { dlopen(cpath.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
    if handle.is_null() {
        return None;
    }
    // A module whose segments the walk could not describe (more than `MAX_LOAD_SEGMENTS`
    // readable ones) gets an empty image: `class_name` on it fails closed, while symbol
    // resolution still works.
    let image = if probe.overflow {
        ImageRange::live(probe.base, 0, Vec::new(), Abi::Itanium)
    } else {
        ImageRange::live(
            probe.base,
            probe.size,
            probe.segments[..probe.count].to_vec(),
            Abi::Itanium,
        )
    };
    Some(Module {
        handle,
        path,
        image,
    })
}

// ---------------------------------------------------------------------------
// Linux: dl_iterate_phdr + dlopen(RTLD_NOLOAD)
// ---------------------------------------------------------------------------

// dlopen/dlclose/dlsym (libdl, in glibc since 2.34 — the image runs 2.41). Declared here like
// the probe does; the mode flags come from `libc` so no hand-written RTLD values (see the
// WP7a note: 0x4000 is NOT RTLD_NOLOAD on glibc).
#[cfg(not(windows))]
extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[cfg(not(windows))]
impl Module {
    /// Resolve an exported symbol (`dlsym`). `None` when the symbol does not exist or the
    /// name contains a NUL byte.
    #[must_use]
    pub fn symbol(&self, name: &str) -> Option<*mut c_void> {
        let cname = CString::new(name).ok()?;
        // SAFETY: `self.handle` is a live dlopen handle; `cname` outlives the call.
        let p = unsafe { dlsym(self.handle, cname.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }
}

#[cfg(not(windows))]
impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: the handle came from dlopen and is still live; for a NOLOAD lookup this only
        // releases the extra reference we took, the game's own mapping stays.
        unsafe {
            dlclose(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: GetModuleHandleW + GetProcAddress
// ---------------------------------------------------------------------------

#[cfg(windows)]
impl Module {
    /// Find a lib already mapped in this process by file name (e.g. `"schemasystem.dll"`, or
    /// [`names`]): `GetModuleHandleW(name)` for the handle (it never loads, exactly like
    /// `dlopen(RTLD_NOLOAD)`), then `GetModuleFileNameW` for the full mapped path. `None` when
    /// no module with that name is mapped.
    ///
    /// Non-blocking: two Win32 calls, no retries, no sleeps — the caller polls on its own
    /// deadline. Matches a bare name (`"server.dll"`) or a full path the way
    /// `GetModuleHandleW` itself does.
    #[must_use]
    pub fn find_loaded(file_name: &str) -> Option<Module> {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};

        let wide: Vec<u16> = file_name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string for the duration of the call;
        // `GetModuleHandleW` only reads it.
        let handle = unsafe { GetModuleHandleW(wide.as_ptr()) };
        if handle.is_null() {
            return None;
        }
        // Draw the mapped path with a growing buffer: `GetModuleFileNameW` returns the buffer
        // size (and truncates) when it does not fit, so a short result means "complete".
        let mut buf = vec![0u16; 260]; // MAX_PATH
        loop {
            // SAFETY: `handle` is a live module handle and `buf` is writable for its length.
            let len = unsafe { GetModuleFileNameW(handle, buf.as_mut_ptr(), buf.len() as u32) };
            if len == 0 {
                return None;
            }
            let len = len as usize;
            if len < buf.len() {
                buf.truncate(len);
                // The loader's path is a wide OS string: convert exactly, not lossily.
                use std::os::windows::ffi::OsStringExt;
                let path = PathBuf::from(std::ffi::OsString::from_wide(&buf));
                // SAFETY: `handle` is the live module handle `GetModuleHandleW` just returned.
                let image = unsafe { live_image(handle.cast::<u8>()) };
                return Some(Module {
                    handle,
                    path,
                    image,
                });
            }
            if buf.len() >= 32_768 {
                return None; // beyond any real path length; do not loop forever
            }
            buf.resize(buf.len() * 2, 0);
        }
    }

    /// Find the **game's own** image mapped under `file_name` (e.g. [`names::SERVER`]): the one
    /// finder for a pinned module, over every image the loader has mapped of that name
    /// ([`mapped`]) and the rules in [`select_module`].
    ///
    /// This is the answer to "which `server.dll` is the game's": Metamod maps a loader stub of
    /// the same name first, and [`Module::find_loaded`]'s `GetModuleHandleW` answers that stub
    /// (`GetModuleHandleW` returns the first module whose base name matches). `Ok(None)` means
    /// nothing is mapped under the name; a [`Reject`] means something is, but not the
    /// game's image (or not unambiguously), and the caller must fail closed.
    ///
    /// Non-blocking: one `K32EnumProcessModules` pass, one `GetModuleFileNameW` per module, no
    /// retries, no sleeps.
    pub fn find_game(file_name: &str) -> Result<Option<Module>, Reject> {
        let candidates = mapped(file_name);
        let host = host_executable();
        let Some(chosen) = select_module(file_name, host.as_deref(), &candidates)? else {
            return Ok(None);
        };
        Ok(module_from(chosen.base, &chosen.path))
    }

    /// Resolve an exported symbol (`GetProcAddress`). `None` when the symbol does not exist
    /// or the name contains a NUL byte.
    #[must_use]
    pub fn symbol(&self, name: &str) -> Option<*mut c_void> {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;

        let cname = CString::new(name).ok()?;
        // SAFETY: `self.handle` is a live module handle from `GetModuleHandleW`; `cname` is a
        // NUL-terminated ANSI string that outlives the call.
        let p = unsafe { GetProcAddress(self.handle, cname.as_ptr().cast::<u8>()) };
        // `FARPROC` is `Option<unsafe extern "system" fn() -> isize>`; a null answer is `None`.
        p.map(|p| p as *mut c_void)
    }
}

/// The [`Module`] for an image `K32EnumProcessModules` listed: its handle (the load address),
/// the path the loader holds, and the image read out of the mapping.
#[cfg(windows)]
fn module_from(base: u64, path: &str) -> Option<Module> {
    let handle = base as usize as *mut c_void;
    if handle.is_null() {
        return None;
    }
    Some(Module {
        handle,
        path: PathBuf::from(path),
        // SAFETY: `base` is a load address the loader's own module list answered, so it is a
        // mapped PE image for the life of the process.
        image: unsafe { live_image(handle.cast::<u8>()) },
    })
}

// ---------------------------------------------------------------------------
// Windows: the live image from the PE header block
// ---------------------------------------------------------------------------

/// `IMAGE_DOS_HEADER::e_lfanew` (`winnt.h`).
#[cfg(windows)]
const PE_E_LFANEW: usize = 0x3c;
/// The largest `e_lfanew` this adapter follows before it has read `SizeOfHeaders`: a real PE's
/// DOS stub plus NT headers live in the first page of the mapping, so this keeps every read
/// that sizes the header block inside bytes the loader certainly mapped.
#[cfg(windows)]
const PE_MAX_HEADER_OFFSET: usize = 0x800;
/// `IMAGE_FILE_HEADER::NumberOfSections` (`IMAGE_FILE_HEADER` starts at `e_lfanew + 4`, +2).
#[cfg(windows)]
const PE_NUMBER_OF_SECTIONS: usize = 6;
/// `IMAGE_FILE_HEADER::SizeOfOptionalHeader` (+16).
#[cfg(windows)]
const PE_SIZE_OF_OPTIONAL_HEADER: usize = 20;
/// `IMAGE_NT_HEADERS64::OptionalHeader`: 4 (signature) + 20 (`IMAGE_FILE_HEADER`).
#[cfg(windows)]
const PE_OPTIONAL_HDR: usize = 24;
/// `IMAGE_OPTIONAL_HEADER64::SizeOfImage` (56 bytes into the optional header).
#[cfg(windows)]
const PE32PLUS_SIZEOFIMAGE: usize = 56;
/// `IMAGE_OPTIONAL_HEADER64::SizeOfHeaders` (+60).
#[cfg(windows)]
const PE32PLUS_SIZEOFHEADERS: usize = 60;
/// `IMAGE_SECTION_HEADER` and the four fields the readable ranges need.
#[cfg(windows)]
const PE_SECTION_HEADER_BYTES: usize = 40;
#[cfg(windows)]
const PE_SECTION_VIRTUAL_SIZE: usize = 8;
#[cfg(windows)]
const PE_SECTION_VIRTUAL_ADDRESS: usize = 12;
#[cfg(windows)]
const PE_SECTION_RAW_SIZE: usize = 16;
#[cfg(windows)]
const PE_SECTION_CHARACTERISTICS: usize = 36;
/// `IMAGE_SCN_MEM_READ`: a section the loader maps readable.
#[cfg(windows)]
const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;

/// The live image of a mapped PE32+ module: `SizeOfImage` bytes at `base`, with every
/// `IMAGE_SCN_MEM_READ` section (and the header block, `SizeOfHeaders`) as a readable range.
///
/// A mapping whose header block does not parse (not PE32+, `e_lfanew` out of bounds, a section
/// table that leaves the header block, a header block larger than the image) yields an image of
/// size 0 with no readable range: every RTTI lookup on it fails closed with
/// [`RttiError::OutsideRva`](crate::rtti::RttiError::OutsideRva) rather than guessing, and
/// symbol resolution is unaffected.
///
/// # Safety
///
/// `base` must be the load address of a PE image the loader has mapped, and that mapping must
/// stay alive for the call (`GetModuleHandleW`'s answer: a mapped module).
#[cfg(windows)]
unsafe fn live_image(base: *const u8) -> ImageRange<'static> {
    let address = base as usize as u64;
    let empty = || ImageRange::live(address, 0, Vec::new(), Abi::Msvc);
    // SAFETY: the DOS header is the first committed page of a mapped image, and `PE_E_LFANEW`
    // is inside it; `e_lfanew` is then bounded by `PE_MAX_HEADER_OFFSET` before the NT header
    // is read, so every read before `SizeOfHeaders` is known stays in the header block.
    let pe = unsafe { read_u32(base, PE_E_LFANEW) };
    let pe = usize::try_from(pe).unwrap_or(usize::MAX);
    if pe == 0 || pe >= PE_MAX_HEADER_OFFSET {
        return empty();
    }
    // SAFETY: `pe` is inside the header block of a mapped PE image (see above).
    if unsafe { read_u32(base, pe) } != u32::from_le_bytes(*b"PE\0\0") {
        return empty();
    }
    // SAFETY: `IMAGE_NT_HEADERS64::OptionalHeader::Magic` is inside the header block.
    if unsafe { read_u16(base, pe + PE_OPTIONAL_HDR) } != 0x20b {
        return empty(); // PE32+ only: the reader is the x64 layout
    }
    // SAFETY: `IMAGE_FILE_HEADER` and `IMAGE_OPTIONAL_HEADER64` size fields, all in the header
    // block (`pe + 4 + 20 + 60 + 4` is inside the first page `pe` was bounded into).
    let (sections, optional_size, size_of_image, size_of_headers) = unsafe {
        (
            usize::from(read_u16(base, pe + PE_NUMBER_OF_SECTIONS)),
            usize::from(read_u16(base, pe + PE_SIZE_OF_OPTIONAL_HEADER)),
            u64::from(read_u32(base, pe + PE_OPTIONAL_HDR + PE32PLUS_SIZEOFIMAGE)),
            u64::from(read_u32(
                base,
                pe + PE_OPTIONAL_HDR + PE32PLUS_SIZEOFHEADERS,
            )),
        )
    };
    let optional = pe + PE_OPTIONAL_HDR;
    let table = optional.saturating_add(optional_size);
    // The section table must be inside the header block the loader mapped (and the header block
    // inside the image): that is the bound for every section read below.
    let table_end = table.saturating_add(sections.saturating_mul(PE_SECTION_HEADER_BYTES));
    if size_of_headers == 0 || size_of_headers > size_of_image || table_end as u64 > size_of_headers
    {
        return empty();
    }
    let mut readable = Vec::with_capacity(sections + 1);
    readable.push((0, size_of_headers));
    for index in 0..sections {
        let header = table + index * PE_SECTION_HEADER_BYTES;
        // SAFETY: the whole section table is `table_end <= SizeOfHeaders` bytes, all inside the
        // mapped header block (checked just above), and `header + 40` is inside the table.
        let (virtual_size, virtual_address, raw_size, characteristics) = unsafe {
            (
                u64::from(read_u32(base, header + PE_SECTION_VIRTUAL_SIZE)),
                u64::from(read_u32(base, header + PE_SECTION_VIRTUAL_ADDRESS)),
                u64::from(read_u32(base, header + PE_SECTION_RAW_SIZE)),
                read_u32(base, header + PE_SECTION_CHARACTERISTICS),
            )
        };
        if characteristics & IMAGE_SCN_MEM_READ == 0 {
            continue;
        }
        let length = if virtual_size == 0 {
            raw_size
        } else {
            virtual_size
        };
        let Some(end) = virtual_address.checked_add(length) else {
            continue;
        };
        if length == 0 || end > size_of_image {
            continue;
        }
        readable.push((virtual_address, length));
    }
    ImageRange::live(address, size_of_image, readable, Abi::Msvc)
}

/// # Safety
///
/// `offset..offset + 4` must be inside a mapped image (the header block: see [`live_image`]).
#[cfg(windows)]
unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
    // SAFETY: the caller guarantees the range is mapped; the header's alignment is not
    // promised, so the read is unaligned.
    unsafe { core::ptr::read_unaligned(base.add(offset).cast::<u32>()) }
}

/// # Safety
///
/// `offset..offset + 2` must be inside a mapped image (the header block: see [`live_image`]).
#[cfg(windows)]
unsafe fn read_u16(base: *const u8, offset: usize) -> u16 {
    // SAFETY: as `read_u32`.
    unsafe { core::ptr::read_unaligned(base.add(offset).cast::<u16>()) }
}

// ---------------------------------------------------------------------------
// Both targets
// ---------------------------------------------------------------------------

impl Module {
    /// The full mapped path this module was found under.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mapped image of this module, for [`crate::rtti::class_name`]: the load address, the
    /// mapped size and the readable RVA ranges the loader described when the module was found.
    ///
    /// No loader call, no file read and no image copy happen here: the numbers were computed
    /// once in [`Module::find_loaded`]. The image reads in place and fails closed (every read
    /// is checked against those ranges), so a caller does not need the module's file.
    #[must_use]
    pub fn image(&self) -> &ImageRange<'static> {
        &self.image
    }

    /// Call the module's `CreateInterface` factory (upstream
    /// `CreateInterfaceFn`: `void *(*)(const char *, int *)` from `public/tier0/interface.h`).
    /// `None` when the module exports no factory, the version string contains a NUL byte, or
    /// the factory returns null.
    ///
    /// `extern "C"` is the factory's actual ABI on both targets: it is a plain C function
    /// pointer, and on `x86_64-pc-windows-msvc` `extern "C"` is the MS x64 ABI.
    #[must_use]
    pub fn create_interface(&self, version: &str) -> Option<core::ptr::NonNull<c_void>> {
        type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
        let sym = self.symbol("CreateInterface")?;
        // SAFETY: `sym` is non-null here and points at the factory's code; the signature is
        // upstream's `CreateInterfaceFn`.
        let factory: CreateInterfaceFn = unsafe { core::mem::transmute(sym) };
        let cversion = CString::new(version).ok()?;
        let mut ret: c_int = 0;
        // SAFETY: the factory is a plain function call; the version string and the return-code
        // out-parameter outlive the call. A null return is the engine's "no such interface".
        let p = unsafe { factory(cversion.as_ptr(), &mut ret) };
        core::ptr::NonNull::new(p)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    /// `libc.so.6` is mapped in every process: find it by file name, resolve `malloc`.
    #[test]
    fn find_loaded_libc_and_malloc_symbol() {
        let module = Module::find_loaded("libc.so.6")
            .expect("libc.so.6 is mapped in every process that runs these tests");
        assert_eq!(
            module.path().file_name(),
            Some(std::ffi::OsStr::new("libc.so.6"))
        );
        let malloc = module.symbol("malloc").expect("libc exports malloc");
        assert!(!malloc.is_null());
    }

    /// A library that is definitely not mapped returns `None`, and a missing symbol too.
    #[test]
    fn find_loaded_missing_and_missing_symbol() {
        assert!(Module::find_loaded("s2sdk-definitely-not-mapped.so").is_none());
        let module =
            Module::find_loaded("libc.so.6").expect("libc.so.6 is mapped in every process");
        assert!(module.symbol("s2sdk_no_such_symbol_42").is_none());
    }

    /// No library exports the game interfaces here (unit tests run outside CS2):
    /// `create_interface` must come up empty, not crash.
    #[test]
    fn create_interface_none_outside_the_game() {
        let module = Module::find_loaded("libc.so.6").expect("libc mapped");
        assert!(module
            .create_interface(versions::SCHEMASYSTEM_INTERFACE_VERSION)
            .is_none());
    }

    /// The per-target name table is what the rest of the crate goes through: on this target
    /// (Linux) the stems carry the `lib…so` spelling.
    #[test]
    fn names_are_the_linux_spelling() {
        assert_eq!(names::SCHEMASYSTEM, "libschemasystem.so");
        assert_eq!(names::SERVER, "libserver.so");
        assert_eq!(names::TIER0, "libtier0.so");
        assert_eq!(names::ENGINE2, "libengine2.so");
    }

    /// The live image of a mapped library describes the mapping the loader made: base 0 is
    /// non-zero, the ELF magic is readable through it, a resolved symbol's address is inside
    /// it, and a read past the end fails closed.
    #[test]
    fn image_describes_the_mapped_library() {
        let module = Module::find_loaded("libc.so.6").expect("libc.so.6 is mapped");
        let image = module.image();
        assert_eq!(image.abi(), Abi::Itanium, "a Linux module is an ELF image");
        assert_ne!(image.base(), 0, "a shared library has a load address");
        assert!(image.size() > 0, "libc has a mapped size");
        assert!(
            !image.readable().is_empty(),
            "the PT_LOAD segments are readable ranges"
        );

        // Reading the header through the image is a read of the mapping: the ELF magic.
        let mut magic = [0u8; 4];
        image.read(0, &mut magic).expect("the header is mapped");
        assert_eq!(&magic, b"\x7fELF");

        // A resolved symbol is inside the image span (so `base`/`size` are the real mapping).
        let malloc = module.symbol("malloc").expect("libc exports malloc") as usize as u64;
        assert!(
            malloc >= image.base() && malloc < image.end(),
            "malloc {malloc:#x} is not inside [{:#x}, {:#x})",
            image.base(),
            image.end()
        );

        // One byte past the end, and a range that starts before the base, are both refused
        // (there is no read to fault, the check comes first).
        let mut byte = [0u8; 1];
        assert!(image.read(image.size(), &mut byte).is_err());
        assert!(image.read(image.size() - 1, &mut [0u8; 2]).is_err());
    }

    // -------------------------------------------------------------------------------------
    // The game-module finder: `select_module` / `game_root` (the rules, on host paths) and
    // `Module::find_game` (the same rules over this process's real mappings).
    // -------------------------------------------------------------------------------------

    const HOST: &str = r"Z:\cs2\game\bin\win64\cs2.exe";
    /// The real Metamod layout (review N3): the stub under `csgo\addons\metamod`, the game's own
    /// under `csgo\bin`.
    const STUB: &str = r"Z:\cs2\game\csgo\addons\metamod\bin\win64\server.dll";
    const GAME: &str = r"Z:\cs2\game\csgo\bin\win64\server.dll";

    fn image(base: u64, path: &str) -> LoadedModule {
        LoadedModule {
            base,
            path: path.to_string(),
        }
    }

    /// The game root is the host executable's directory with the loader directories removed:
    /// `bin\win64` on Windows, `bin/linuxsteamrt64` on Linux (review N3).
    #[test]
    fn the_game_root_drops_the_loader_directories() {
        assert_eq!(
            game_root(HOST).as_deref(),
            Some(r"Z:\cs2\game"),
            "the host executable's two loader directories"
        );
        assert_eq!(
            game_root("/opt/cs2/game/bin/linuxsteamrt64/cs2").as_deref(),
            Some("/opt/cs2/game"),
            "the Linux install's loader directory"
        );
        assert_eq!(
            game_root("/opt/cs2/game/bin/cs2").as_deref(),
            Some("/opt/cs2/game"),
            "a plain bin directory"
        );
        // A bare name has no directory to strip.
        assert_eq!(game_root("cs2.exe"), None);
        assert_eq!(game_root(""), None);
    }

    /// Two images named `server.dll` in the real layout: Metamod's stub is first (it is loaded
    /// first), and the game's own binary wins - `GetModuleHandleA` would have answered the stub.
    #[test]
    fn the_game_image_wins_over_metamods_stub() {
        let candidates = [image(0x1000_0000, STUB), image(0x2000_0000, GAME)];
        let chosen = select_module("server.dll", Some(HOST), &candidates)
            .expect("two images, one usable")
            .expect("one is chosen");
        assert_eq!(chosen.path, GAME, "the game's own server.dll");
        assert_eq!(chosen.base, 0x2000_0000);
    }

    /// The Linux twin of the same layout: `<game>/csgo/addons/metamod/bin/linuxsteamrt64/
    /// libserver.so` (the stub) versus `<game>/csgo/bin/linuxsteamrt64/libserver.so`.
    #[test]
    fn the_linux_layout_picks_the_game_libserver() {
        let host = "/opt/cs2/game/bin/linuxsteamrt64/cs2";
        let stub = "/opt/cs2/game/csgo/addons/metamod/bin/linuxsteamrt64/libserver.so";
        let game = "/opt/cs2/game/csgo/bin/linuxsteamrt64/libserver.so";
        let candidates = [image(0x1000_0000, stub), image(0x2000_0000, game)];
        let chosen = select_module("libserver.so", Some(host), &candidates)
            .expect("two images, one usable")
            .expect("one is chosen");
        assert_eq!(chosen.path, game, "the game's own libserver.so");
    }

    /// Only the stub mapped: nothing is chosen, and the reason says why.
    #[test]
    fn a_lone_loader_stub_is_refused() {
        let candidates = [image(0x1000_0000, STUB)];
        assert_eq!(
            select_module("server.dll", Some(HOST), &candidates),
            Err(Reject::LoaderStub)
        );
        // Without the game root the stub is still excluded (the `metamod` component is enough).
        assert_eq!(
            select_module("server.dll", None, &candidates),
            Err(Reject::LoaderStub)
        );
    }

    /// An install that itself lives under a `metamod` directory still finds its own image
    /// (review N1): the `metamod` rule reads only the part below the game root.
    #[test]
    fn an_install_under_a_metamod_directory_is_not_refused() {
        let host = r"D:\metamod\cs2\game\bin\win64\cs2.exe";
        let game = r"D:\metamod\cs2\game\csgo\bin\win64\server.dll";
        let candidates = [image(0x2000_0000, game)];
        let chosen = select_module("server.dll", Some(host), &candidates)
            .expect("the install's own path is not the stub rule's business")
            .expect("the only image is the game's");
        assert_eq!(chosen.path, game);
        // And the stub of that install is still refused.
        let stub = r"D:\metamod\cs2\game\csgo\addons\metamod\bin\win64\server.dll";
        assert_eq!(
            select_module("server.dll", Some(host), &[image(0x1000_0000, stub)]),
            Err(Reject::LoaderStub)
        );
    }

    /// Nothing mapped under the name is the C's "not loaded", not a refusal; two images under
    /// the root are refused rather than guessed.
    #[test]
    fn nothing_mapped_is_not_loaded_and_two_game_images_are_ambiguous() {
        assert_eq!(select_module("server.dll", Some(HOST), &[]), Ok(None));
        // An image under a *different* name is not a candidate (`mapped` filters by name, and
        // the name filter is applied again here).
        let other = [image(0x1000_0000, r"Z:\cs2\game\csgo\bin\win64\client.dll")];
        assert_eq!(select_module("server.dll", Some(HOST), &other), Ok(None));
        let two = [
            image(0x2000_0000, GAME),
            image(0x3000_0000, r"Z:\cs2\game\csgo\bin\win64\sub\server.dll"),
        ];
        assert_eq!(
            select_module("server.dll", Some(HOST), &two),
            Err(Reject::Ambiguous)
        );
    }

    /// A relocated install (nothing under the derived root) still works with one image, and a
    /// candidate without a path - what a test double answers - is taken as the only one.
    #[test]
    fn one_image_outside_the_root_or_without_a_path_is_taken() {
        let outside = [image(
            0x2000_0000,
            r"D:\elsewhere\csgo\bin\win64\server.dll",
        )];
        assert_eq!(
            select_module("server.dll", Some(HOST), &outside)
                .expect("one image, nothing to choose")
                .map(|chosen| chosen.base),
            Some(0x2000_0000)
        );
        let pathless = [LoadedModule {
            base: 0x4000_0000,
            path: String::new(),
        }];
        assert_eq!(
            select_module("server.dll", Some(HOST), &pathless)
                .expect("one image, nothing to choose")
                .map(|chosen| chosen.base),
            Some(0x4000_0000)
        );
    }

    /// The finder over this process's real mappings: a library that is mapped once is found
    /// through `mapped` + `select_module` + the open, and a name nothing carries is `Ok(None)`.
    #[test]
    fn find_game_answers_the_games_image_or_none() {
        let mapped_libc = mapped("libc.so.6");
        assert_eq!(mapped_libc.len(), 1, "one libc mapping in this process");
        assert!(mapped_libc[0].base != 0 && !mapped_libc[0].path.is_empty());
        let module = Module::find_game("libc.so.6")
            .expect("one image, one candidate")
            .expect("libc.so.6 is mapped in every process");
        assert_eq!(
            module.path().file_name(),
            Some(std::ffi::OsStr::new("libc.so.6"))
        );
        assert_eq!(module.image().base(), mapped_libc[0].base);
        assert!(Module::find_game("s2sdk-definitely-not-mapped.so")
            .expect("nothing is mapped under the name")
            .is_none());
    }

    /// The host executable's path is a real path, and its root is a prefix of its own directory
    /// (the anchor `game_root` derives, over this process).
    #[test]
    fn the_host_executable_is_named() {
        let host = host_executable().expect("/proc/self/exe always resolves");
        assert!(host.starts_with('/'), "{host}");
        let root = game_root(&host).expect("an absolute path has a directory");
        assert!(
            host.starts_with(root.as_str()),
            "the root {root} is a prefix of the executable {host}"
        );
    }
}
