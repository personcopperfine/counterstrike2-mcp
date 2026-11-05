//! Game events: register a Rust-owned `IGameEventListener2` with the engine's
//! `IGameEventManager2`, and read event payloads through the generated `IGameEvent_vtable`.
//!
//! The listener is a Rust object (a private `ListenerObject`) whose vtable is the generated
//! `IGameEventListener2_vtable` filled with trampolines: the engine calls into Rust, Rust
//! calls the boxed closure. No Valve code is patched — the object registers through the
//! game's own `AddListener` API exactly like a C++ listener would.
//!
//! # Getting `IGameEventManager2` (two strategies)
//!
//! * **Strategy A (implemented here):** `CreateInterface("GAMEEVENTSMANAGER002")` on the
//!   `engine2` module (per target: `libengine2.so` / `engine2.dll` — see
//!   [`crate::module::names`]) — see [`GameEventManager::from_process`]. **Live-verified
//!   unavailable on the Linux dedicated server (WP6b):** `libengine2.so` exports
//!   `CreateInterface` but none of the server's modules (`libengine2.so`, `libserver.so`,
//!   `libtier0.so`) carries the version string; only `libclient.so` and `libmatchmaking.so`
//!   do. The probe's `events` check reports the failure (non-mandatory).
//! * **Strategy B (still to do):** CS2Fixes reads the manager from an `ISource2Server` virtual
//!   and subtracts 8 (the slot immediately before it in the server class layout). That needs
//!   `ISource2Server` in the IR, which the current allowlist does not cover.
//!
//! Live availability of strategy A on the Linux dedicated server is **disproved** by the WP6b
//! probe: the interface name resolves to nothing there. Strategy B (or a client-side probe,
//! where `libclient.so` carries the string) is the way forward.
//!
//! # Thread-safety contract (inherited from the engine)
//!
//! `AddListener` / `RemoveListener` must be called on the game thread; events fire on the
//! game thread too. [`GameEventManager`] is therefore `!Send + !Sync` (a
//! `PhantomData<*mut ()>`), and [`Listener`] with it: keep them on the thread that created
//! them, and drop the [`Listener`] there (its `Drop` calls `RemoveListener` — documented at
//! [`Listener`]).
//!
//! Two further rules enforced by [`GameEventManager::add_listener`]'s fire trampoline:
//! a re-entrant `FireGameEvent` of the *same* listener is skipped (debug builds assert),
//! and dropping a [`Listener`] from inside its own callback is **not allowed** (the
//! trampoline keeps the object alive, but the unregister-while-dispatching sequence is
//! unsafe upstream behaviour).

use core::cell::Cell;
use core::ffi::CStr;
use core::marker::PhantomData;
use std::ffi::CString;
use std::ptr::NonNull;
use std::rc::Rc;

use s2sdk_sys::generated as sys;

use crate::error::{Error, Result};
use crate::hash::{murmur_hash2_lowercase, STRINGTOKEN_MURMURHASH_SEED};
use crate::interface::{create, Interface};
use crate::module::{versions, Module};

/// `UTL_INVAL_SYMBOL_LARGE` from `public/tier0/utlsymbollarge.h`:
/// `((UtlSymLargeId_t)~0)` — the "no string-table symbol yet" marker `CKV3MemberName`'s
/// constructors put into `m_SymId`.
const UTL_INVAL_SYMBOL_LARGE: u32 = u32::MAX;

/// A game-event key: the `const GameEventKeySymbol_t&` (= `const CKV3MemberName&`) the
/// generated `IGameEvent_vtable` getters take, built in Rust.
///
/// Mirrors upstream's `CKV3MemberName(const char *)` constructor
/// (`public/tier0/keyvalues3.h`): `m_nHashCode` is `MurmurHash2LowerCase(name,
/// STRINGTOKEN_MURMURHASH_SEED)` — except for a null or *empty* name, where the hash code
/// stays 0; `m_SymId` is `UTL_INVAL_SYMBOL_LARGE`; `m_pszString` points at the name.
///
/// `'a` is the lifetime of the name the key points at: the struct borrows the `CStr`
/// (via `PhantomData`) while staying [`Copy`], so a `KeySymbol` cannot be held past the
/// end of the name — event keys are static strings in practice, and every `GameEvent`
/// getter takes `&KeySymbol` only for the duration of its call.
#[derive(Clone, Copy)]
pub struct KeySymbol<'a>(sys::CKV3MemberName, PhantomData<&'a CStr>);

impl<'a> core::fmt::Debug for KeySymbol<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `CKV3MemberName` has no generated Debug; print the hash and symbol id.
        f.debug_struct("KeySymbol")
            .field("hash", &self.0.m_nHashCode.m_nHashCode)
            .field("sym_id", &self.0.m_SymId)
            .finish()
    }
}

impl<'a> KeySymbol<'a> {
    /// Build a key for `name`, hashed exactly like upstream's string token. The key borrows
    /// `name`: the `KeySymbol` cannot outlive it (the engine reads `m_pszString` for the
    /// duration of the getter call).
    ///
    /// # Errors
    ///
    /// [`Error::Utf8`] when the name is longer than `i32::MAX` (the upstream hash takes an
    /// `int` length; not reachable for event key names in practice).
    pub fn new(name: &'a CStr) -> Result<Self> {
        let bytes = name.to_bytes();
        if bytes.len() > i32::MAX as usize {
            // The upstream hash takes an `int` length; a name that long cannot be hashed
            // through the same path (unreachable for event keys in practice).
            return Err(Error::Utf8);
        }
        // Upstream: `if(!pszString || !pszString[0]) return;` — hash code stays 0 for an
        // empty name (a null `CStr` cannot be constructed in safe Rust, so only the empty
        // case applies here).
        let hash = if bytes.is_empty() {
            0
        } else {
            murmur_hash2_lowercase(bytes, STRINGTOKEN_MURMURHASH_SEED)
        };
        Ok(Self(
            sys::CKV3MemberName {
                // SAFETY: field construction of a plain generated record (no unsafe semantics).
                m_nHashCode: sys::CUtlStringToken { m_nHashCode: hash },
                m_SymId: UTL_INVAL_SYMBOL_LARGE,
                m_pszString: name.as_ptr(),
            },
            PhantomData,
        ))
    }

    /// The token hash this key carries (what the engine compares).
    #[must_use]
    pub fn hash(&self) -> u32 {
        self.0.m_nHashCode.m_nHashCode
    }

    /// The symbol-table id the key carries: always `u32::MAX` (`UTL_INVAL_SYMBOL_LARGE` in
    /// upstream) for keys built here, since we never register a symbol.
    #[must_use]
    pub fn sym_id(&self) -> u32 {
        self.0.m_SymId
    }
}

/// The Rust-owned listener object passed to the engine as an `IGameEventListener2*`.
///
/// `#[repr(C)]` with the vtable pointer first, so the object *is* a valid
/// `IGameEventListener2` from the engine's side (same address-point trick as any C++
/// single-inheritance listener object; see the brief's note: no offset-to-top/RTTI words,
/// which only breaks `dynamic_cast` — the engine does not cast listeners).
///
/// Shared with the engine through an `Rc` (not `Arc`: the whole game-event layer is
/// game-thread-only — see `GameEventManager` — so the non-atomic refcount matches the
/// contract): the engine holds a raw pointer for as long as
/// the listener is registered, so the object must outlive the owning [`Listener`] even in
/// the (documented as disallowed) case of a callback dropping its own [`Listener`] while
/// the trampoline borrows the callback. The `Rc` keeps the allocation alive for the whole
/// fire — the [`busy`](ListenerObject::busy) flag writes below therefore never touch freed
/// memory.
#[repr(C)]
struct ListenerObject {
    /// The static Rust vtable below; the address the engine stores.
    vtable: *const sys::IGameEventListener2_vtable,
    /// The user callback, boxed for a stable layout inside the object.
    callback: Box<dyn FnMut(&GameEvent)>,
    /// Re-entrancy guard: set for the duration of one fire. A nested `FireGameEvent` for
    /// the same listener is skipped. Accessed through `Rc` clones (the allocation is kept
    /// alive by the trampoline while the flag is set).
    busy: Cell<bool>,
}

/// The listener vtable shared by every [`ListenerObject`]: the destructor slot(s) (nothing
/// owns the object C++-side) and the fire trampoline.
///
/// The two targets need different *shapes*, exactly as the generated
/// [`sys::IGameEventListener2_vtable`] does (row R4):
///
/// * **Itanium** (Linux): a virtual destructor takes **two** slots — `dtor_complete` then
///   `dtor_deleting` — so `FireGameEvent` sits at slot 2.
/// * **MSVC** (Windows): one **scalar-deleting** destructor slot (`dtor`) — the
///   compiler-generated `void* (this, unsigned int flags)` wrapper — so `FireGameEvent`
///   sits at slot 1. The generated binding models that wrapper's signature exactly.
///
/// In both cases the entry positions come from the generated struct, never from a slot number
/// written here.
#[cfg(not(windows))]
static LISTENER_VTABLE: sys::IGameEventListener2_vtable = sys::IGameEventListener2_vtable {
    dtor_complete: Some(noop_dtor),
    dtor_deleting: Some(noop_dtor),
    FireGameEvent: Some(fire_trampoline),
};

/// MSVC twin: one scalar-deleting destructor slot (`dtor`), then `FireGameEvent` (slot 1).
#[cfg(windows)]
static LISTENER_VTABLE: sys::IGameEventListener2_vtable = sys::IGameEventListener2_vtable {
    dtor: Some(noop_dtor),
    FireGameEvent: Some(fire_trampoline),
};

/// Destructor slot no-op: the engine never destroys our listener (we unregister it first,
/// then drop it in Rust); the slot(s) exist only to keep the generated layout honest.
#[cfg(not(windows))]
unsafe extern "C" fn noop_dtor(_this: *mut sys::IGameEventListener2) {}

/// MSVC twin of [`noop_dtor`], carrying the scalar-deleting wrapper's real signature
/// (`void* (this, unsigned int flags)`): `flags & 1` would be the engine asking the wrapper
/// to `operator delete` the object, which cannot happen — the object is owned by Rust only
/// (unregistered first, dropped in `Drop`; see [`ListenerObject`]), so any call lands with
/// `flags == 0` and even a deleting call has nothing to free. Like MSVC's own wrapper it
/// returns `this`; see `msvc_dtor_type` for the ABI the generated binding encodes.
#[cfg(windows)]
unsafe extern "C" fn noop_dtor(
    this: *mut sys::IGameEventListener2,
    _flags: u32,
) -> *mut ::core::ffi::c_void {
    this.cast()
}

/// The `FireGameEvent` trampoline the engine calls: cast `this` back to the Rust object and
/// run the boxed callback. Never unwinds into the engine (a panic here is caught, FFI
/// boundary rule).
unsafe extern "C" fn fire_trampoline(
    this: *mut sys::IGameEventListener2,
    event: *mut sys::IGameEvent,
) {
    let Some(event) = NonNull::new(event) else {
        return; // engine handed us a null event: nothing to report, never crash
    };
    let event = GameEvent(event);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `this` is the pointer `add_listener` handed the engine (`Rc::into_raw`
        // of a `ListenerObject`), and a fire implies the listener is still registered —
        // `RemoveListener` ran nowhere else (game-thread-only contract) and `Drop` runs it
        // *before* releasing the object. Re-borrowing here adds an owned strong count that
        // is released at the end of the closure: the object stays alive even if the
        // callback drops its own `Listener` mid-fire (that remains documented as
        // disallowed, but it can no longer free the object under the running callback).
        // The increment comes first: `from_raw` alone would adopt the *engine's* count and
        // release it when `shared` drops, freeing the object after the second fire.
        let shared: Rc<ListenerObject> = unsafe {
            Rc::increment_strong_count(this as *const ListenerObject);
            Rc::from_raw(this as *const ListenerObject)
        };
        // Re-entrancy: a callback that dispatches work the engine answers with the same
        // event again would re-enter this listener. Nested fires are skipped so the
        // `&mut` callback borrow is never held twice; a debug build asserts instead of
        // skipping silently. The guard is temporary state on the shared object; the
        // `Rc` borrow above guarantees the memory is live for the write-back.
        // (Aliasing: `busy` is `Cell<bool>` accessed only here, per the game-thread-only
        // fire contract.)
        if shared.busy.get() {
            debug_assert!(
                false,
                "re-entrant FireGameEvent on the same listener was skipped"
            );
            return;
        }
        shared.busy.set(true);
        struct BusyGuard<'a>(&'a Cell<bool>);
        // Reset on every path, including a panicking callback unwinding through here.
        impl Drop for BusyGuard<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _guard = BusyGuard(&shared.busy);
        // SAFETY: `shared` keeps the object alive (see above); the callback is only
        // reborrowed for the call, never moved. Mutability is exclusive because the
        // guard above skips any nested re-entry of this listener (game-thread-only).
        unsafe { ((&mut *Rc::as_ptr(&shared).cast_mut()).callback)(&event) };
    }));
    // A panicking user callback must not unwind through the engine frame: the panic is
    // contained here (the closure sees no event for that dispatch; `busy` is reset by the
    // guard inside).
    drop(result);
}

/// The game event manager interface (`GAMEEVENTSMANAGER002`; see the module docs for the two
/// lookup strategies — live-verified unavailable on the Linux dedicated server).
///
/// **`!Send + !Sync` on purpose** (a `PhantomData<*mut ()>` blocks the blanket impl):
/// `AddListener`, `RemoveListener` and [`Listener`]'s `Drop` mutate the manager's listener
/// list without locking it for arbitrary threads, and events fire on the game thread. Keep
/// the manager, and every [`Listener`] it hands out, on the game thread (create and drop
/// them there; see [`GameEventManager::add_listener`]).
#[derive(Clone, Copy, Debug)]
pub struct GameEventManager(Interface<sys::IGameEventManager2>, PhantomData<*mut ()>);

impl GameEventManager {
    /// Create `GAMEEVENTSMANAGER002` from the `engine2` module (strategy A, the module docs;
    /// per target: `libengine2.so` / `engine2.dll` — see [`crate::module::names`]).
    /// Non-blocking: one `find_loaded` probe + one `CreateInterface` call.
    ///
    /// # Errors
    ///
    /// [`Error::ModuleNotLoaded`] when the `engine2` module is not mapped;
    /// [`Error::InterfaceMissing`] when the factory does not serve the version (live: this is
    /// the Linux dedicated server's path).
    pub fn from_process() -> Result<Self> {
        const LIB: &str = crate::module::names::ENGINE2;
        let module =
            Module::find_loaded(LIB).ok_or_else(|| Error::ModuleNotLoaded(LIB.to_string()))?;
        let version = versions::GAME_EVENTS_MANAGER_INTERFACE_VERSION;
        let iface =
            create(&module, version).ok_or_else(|| Error::InterfaceMissing(version.to_string()))?;
        Ok(Self(iface, PhantomData))
    }

    /// Wrap a raw `IGameEventManager2` pointer (strategy B: a pointer found another way,
    /// e.g. the WP6b/7 fallback).
    ///
    /// # Safety
    ///
    /// `p` must point at a live `IGameEventManager2`; it must outlive every [`Listener`]
    /// registered through the returned manager.
    #[must_use]
    pub unsafe fn from_raw(p: *mut sys::IGameEventManager2) -> Option<Self> {
        // SAFETY: the caller's contract is the wrapper's guarantee.
        unsafe { Interface::from_raw(p).map(|iface| Self(iface, PhantomData)) }
    }

    /// The raw interface pointer.
    #[must_use]
    pub fn as_ptr(&self) -> *mut sys::IGameEventManager2 {
        self.0.as_ptr()
    }

    /// Register `f` for the named event through `AddListener`, and keep the Rust object
    /// alive in the returned [`Listener`].
    ///
    /// The callback runs on the game thread every time the event fires. **This call itself
    /// is game-thread-only** (upstream `AddListener` mutates the manager's listener list):
    /// call it on the thread that owns the [`GameEventManager`], and drop the
    /// [`Listener`] there too.
    ///
    /// Dropping the [`Listener`] from *inside its own callback* is **not allowed** (the
    /// engine is mid-dispatch on the very object being unregistered). The fire trampoline
    /// keeps the object alive regardless (a shared `Rc` re-borrow) and skips a nested
    /// re-entry of the same listener (`debug_assert` in debug builds), but the
    /// registration/drop ordering is only correct on the game thread.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`] when the slot has no typed signature;
    /// [`Error::NotFound`] when `AddListener` returns a negative index (unknown event);
    /// [`Error::Utf8`] for a malformed event name.
    ///
    /// # Panics
    ///
    /// Never: a panicking callback is caught in the fire trampoline.
    pub fn add_listener(
        &self,
        event: &str,
        server_side: bool,
        f: impl FnMut(&GameEvent) + 'static,
    ) -> Result<Listener> {
        // SAFETY: the interface points at a live object (constructor contract).
        let vt = unsafe { sys::IGameEventManager2::vtable(self.0.as_ptr()) };
        let add = vt
            .AddListener
            .ok_or(Error::MissingSlot("IGameEventManager2::AddListener"))?;
        let cname = CString::new(event).map_err(|_| Error::Utf8)?;
        // The object is shared with the engine: one reference goes to the engine (the
        // pointer it stores), one stays here — see [`ListenerObject`].
        let obj = Rc::new(ListenerObject {
            vtable: &LISTENER_VTABLE,
            callback: Box::new(f),
            busy: Cell::new(false),
        });
        // SAFETY: the shared object lives while the engine holds the pointer — the owning
        // `Listener` unregisters it (`RemoveListener`) before releasing its own strong
        // count, and the fire trampoline holds its own count for the duration of a fire.
        // Layout: `#[repr(C)]` with the vtable pointer first, so the cast is the C++ upcast
        // the engine expects.
        let engine_ptr = Rc::into_raw(Rc::clone(&obj))
            .cast_mut()
            .cast::<sys::IGameEventListener2>();
        // SAFETY: the entry's generated signature: `int (this, listener, name, bServerSide)`.
        // Upstream (`igameevents.h`): "returns the event's index or -1 if it's unknown" —
        // any index >= 0 (not only 1) is a registration the engine now holds.
        let ret = unsafe { add(self.0.as_ptr(), engine_ptr, cname.as_ptr(), server_side) };
        let listener = Listener {
            _obj: obj,
            engine_ptr,
            manager: *self,
            event_index: ret,
        };
        if ret < 0 {
            // Unknown event. Dropping the `Listener` calls `RemoveListener` before the
            // object's refcount is released, so the engine can never be left holding a
            // dangling pointer.
            drop(listener);
            return Err(Error::NotFound(event.to_string()));
        }
        Ok(listener)
    }

    /// The numeric id of a named event (`LookupEventId`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::Utf8`].
    pub fn lookup_event_id(&self, name: &str) -> Result<i32> {
        // SAFETY: the interface points at a live object (constructor contract).
        let vt = unsafe { sys::IGameEventManager2::vtable(self.0.as_ptr()) };
        let f = vt
            .LookupEventId
            .ok_or(Error::MissingSlot("IGameEventManager2::LookupEventId"))?;
        let cname = CString::new(name).map_err(|_| Error::Utf8)?;
        // SAFETY: the entry's generated signature: `int (this, name)`.
        Ok(unsafe { f(self.0.as_ptr(), cname.as_ptr()) })
    }

    /// Does an event descriptor with this name exist? (`HasEventDescriptor`.)
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::Utf8`].
    pub fn has_event(&self, name: &str) -> Result<bool> {
        // SAFETY: the interface points at a live object (constructor contract).
        let vt = unsafe { sys::IGameEventManager2::vtable(self.0.as_ptr()) };
        let f = vt
            .HasEventDescriptor
            .ok_or(Error::MissingSlot("IGameEventManager2::HasEventDescriptor"))?;
        let cname = CString::new(name).map_err(|_| Error::Utf8)?;
        // SAFETY: the entry's generated signature: `bool (this, name)`.
        Ok(unsafe { f(self.0.as_ptr(), cname.as_ptr()) })
    }
}

/// A registered listener: owns a share of the Rust object the engine points at, plus the
/// manager handle it needs to unregister.
///
/// `!Send + !Sync` by construction ([`Box<dyn FnMut>`] is neither): the callback fires on
/// the game thread, so keep and drop the [`Listener`] there.
///
/// # Drop behaviour
///
/// Dropping calls `RemoveListener` on the manager. **This must run on the game thread** —
/// upstream mutates the listener list without locking it for arbitrary threads. Drop the
/// [`Listener`] from game-thread code (or from the same thread that called
/// [`GameEventManager::add_listener`]).
///
/// Dropping the [`Listener`] from inside its own callback is not allowed (see
/// [`GameEventManager::add_listener`]): the fire trampoline keeps the shared object alive
/// and no longer frees it under the running callback, but the unregister-while-dispatching
/// sequence itself is unsafe upstream behaviour.
pub struct Listener {
    /// Our share of the shared listener object (the engine holds the other share until we
    /// unregister — see [`Drop`]).
    _obj: Rc<ListenerObject>,
    /// The pointer handed to the engine: `Rc::into_raw` of the engine's own share, taken
    /// exactly once in `add_listener` and given back exactly once in [`Drop`].
    engine_ptr: *mut sys::IGameEventListener2,
    manager: GameEventManager,
    event_index: i32,
}

impl Listener {
    /// The event index `AddListener` returned for this registration.
    #[must_use]
    pub fn event_index(&self) -> i32 {
        self.event_index
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Unregister first (game thread — see the type docs), *then* release refcounts.
        //
        // SAFETY: the interface points at a live object (constructor contract); `ptr` is
        // the exact pointer `add_listener` registered.
        let vt = unsafe { sys::IGameEventManager2::vtable(self.manager.0.as_ptr()) };
        debug_assert!(
            vt.RemoveListener.is_some(),
            "generated IGameEventManager2_vtable has no RemoveListener entry"
        );
        if let Some(remove) = vt.RemoveListener {
            // SAFETY: the entry's generated signature: `void (this, listener)`; the
            // listener pointer is the one the manager stored. After `RemoveListener` the
            // engine no longer uses the pointer — releasing the reference it held is safe.
            unsafe {
                remove(self.manager.0.as_ptr(), self.engine_ptr);
            }
        }
        // The engine's share: `add_listener` transferred one reference with `Rc::into_raw`;
        // `from_raw` takes that same reference back exactly once and releases it here.
        // `RemoveListener` above means the engine no longer dereferences the pointer.
        //
        // SAFETY: `engine_ptr` is the `Rc::into_raw` value from `add_listener`, never
        // reclaimed before, and the engine has just been told to forget it.
        let engine_ref =
            unsafe { Rc::from_raw(self.engine_ptr.cast_const().cast::<ListenerObject>()) };
        drop(engine_ref);
        // Our own strong count drops with `self._obj` after this; the allocation is freed
        // only when the last reference (never the engine's, post-`RemoveListener`) goes
        // away.
    }
}

/// One live game event handed to a listener callback; a view over the engine's
/// `IGameEvent`, valid for the duration of the callback only.
#[derive(Debug)]
pub struct GameEvent(NonNull<sys::IGameEvent>);

impl GameEvent {
    /// Wrap a raw `IGameEvent` pointer (also how the mock tests construct one).
    ///
    /// # Safety
    ///
    /// `p` must point at a live `IGameEvent` owned by the engine for the duration of the
    /// callback.
    #[must_use]
    pub unsafe fn from_raw(p: *mut sys::IGameEvent) -> Option<Self> {
        NonNull::new(p).map(Self)
    }

    /// The event's name (`GetName`), e.g. `"player_death"`.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::NullReturn`], [`Error::Utf8`].
    pub fn name(&self) -> Result<String> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt
            .GetName
            .ok_or(Error::MissingSlot("IGameEvent::GetName"))?;
        // SAFETY: the entry's generated signature: `const char *(this)`.
        let p = unsafe { f(self.0.as_ptr()) };
        crate::schema::cstr_to_string(p)
    }

    /// The event's numeric id (`GetID`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`].
    pub fn id(&self) -> Result<i32> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt.GetID.ok_or(Error::MissingSlot("IGameEvent::GetID"))?;
        // SAFETY: the entry's generated signature: `int (this)`.
        Ok(unsafe { f(self.0.as_ptr()) })
    }

    /// Is the event reliable (network-guaranteed)? (`IsReliable`.)
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`].
    pub fn is_reliable(&self) -> Result<bool> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt
            .IsReliable
            .ok_or(Error::MissingSlot("IGameEvent::IsReliable"))?;
        // SAFETY: the entry's generated signature: `bool (this)`.
        Ok(unsafe { f(self.0.as_ptr()) })
    }

    /// Does the event have no value for `key`? (`IsEmpty`.)
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`].
    pub fn is_empty(&self, key: &KeySymbol) -> Result<bool> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt
            .IsEmpty
            .ok_or(Error::MissingSlot("IGameEvent::IsEmpty"))?;
        // SAFETY: the entry's generated signature; the key outlives the call.
        Ok(unsafe { f(self.0.as_ptr(), &key.0) })
    }

    /// An integer field (`GetInt`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`].
    pub fn get_int(&self, key: &KeySymbol, default: i32) -> Result<i32> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt.GetInt.ok_or(Error::MissingSlot("IGameEvent::GetInt"))?;
        // SAFETY: the entry's generated signature; the key outlives the call.
        Ok(unsafe { f(self.0.as_ptr(), &key.0, default) })
    }

    /// A boolean field (`GetBool`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`].
    pub fn get_bool(&self, key: &KeySymbol, default: bool) -> Result<bool> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt
            .GetBool
            .ok_or(Error::MissingSlot("IGameEvent::GetBool"))?;
        // SAFETY: the entry's generated signature; the key outlives the call.
        Ok(unsafe { f(self.0.as_ptr(), &key.0, default) })
    }

    /// A float field (`GetFloat`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`].
    pub fn get_float(&self, key: &KeySymbol, default: f32) -> Result<f32> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt
            .GetFloat
            .ok_or(Error::MissingSlot("IGameEvent::GetFloat"))?;
        // SAFETY: the entry's generated signature; the key outlives the call.
        Ok(unsafe { f(self.0.as_ptr(), &key.0, default) })
    }

    /// A 64-bit integer field (`GetUint64`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`].
    pub fn get_u64(&self, key: &KeySymbol, default: u64) -> Result<u64> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt
            .GetUint64
            .ok_or(Error::MissingSlot("IGameEvent::GetUint64"))?;
        // SAFETY: the entry's generated signature; the key outlives the call.
        Ok(unsafe { f(self.0.as_ptr(), &key.0, default) })
    }

    /// A string field (`GetString`), defaulting to empty when the key is absent.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::Utf8`].
    pub fn get_string(&self, key: &KeySymbol) -> Result<String> {
        // SAFETY: the event points at a live object for the callback's duration.
        let vt = unsafe { sys::IGameEvent::vtable(self.0.as_ptr()) };
        let f = vt
            .GetString
            .ok_or(Error::MissingSlot("IGameEvent::GetString"))?;
        const EMPTY: &CStr = c"";
        // SAFETY: the entry's generated signature; the key and the default string outlive
        // the call, and the returned pointer is the engine's own string data.
        let p = unsafe { f(self.0.as_ptr(), &key.0, EMPTY.as_ptr()) };
        if p.is_null() {
            return Ok(String::new());
        }
        // SAFETY: `p` is the engine's NUL-terminated string for the callback's duration.
        let s = unsafe { CStr::from_ptr(p) };
        Ok(s.to_str().map_err(|_| Error::Utf8)?.to_owned())
    }
}
