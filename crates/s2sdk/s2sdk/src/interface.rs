//! Typed pointers to interfaces obtained through a [`Module`]'s `CreateInterface` factory.
//!
//! [`Interface`] is a non-owning, cheaply copied wrapper: the game owns the object. The
//! calling-convention helper used everywhere in this crate:
//!
//! ```ignore
//! let vt = unsafe { ISchemaSystem::vtable(ptr) };
//! let f = vt.FindTypeScopeForModule.ok_or(Error::MissingSlot("ISchemaSystem::FindTypeScopeForModule"))?;
//! let p = unsafe { f(ptr, cname.as_ptr(), ptr::null_mut()) };
//! ```
//!
//! Never `unwrap()` a vtable entry: a `None` means the generated binding has no typed
//! signature for the slot, and that maps to
//! [`Error::MissingSlot`](crate::error::Error::MissingSlot), not to a null transmute.

use core::fmt;
use core::ptr::NonNull;

use crate::module::Module;

/// A typed pointer to a live interface object, obtained via `CreateInterface`.
///
/// `Copy`/`Clone` on purpose: the pointer is not owned (no refcount, no destructor — the game
/// keeps the singleton alive). Call the object's virtuals through the generated `*_vtable`
/// structs.
pub struct Interface<T> {
    pub(crate) ptr: NonNull<T>,
}

impl<T> Clone for Interface<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Interface<T> {}

impl<T> fmt::Debug for Interface<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The type name plus the address; the pointee is opaque C++ data.
        f.debug_struct("Interface")
            .field("ptr", &self.ptr.as_ptr())
            .finish()
    }
}

impl<T> Interface<T> {
    /// Wrap a raw pointer. `None` when null (the caller's "factory returned null" path).
    ///
    /// # Safety
    ///
    /// `p` must point at a live object of (a C++ class derived from) `T` whose vtable pointer
    /// matches the generated `T_vtable` layout, and the object must outlive every copy of the
    /// returned [`Interface`].
    pub unsafe fn from_raw(p: *mut T) -> Option<Self> {
        NonNull::new(p).map(|ptr| Self { ptr })
    }

    /// The raw pointer, for passing as `this` into generated vtable entries.
    #[must_use]
    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }
}

// SAFETY: the wrapped interface objects are process-global singletons (schema system, cvar
// system) the engine itself shares across threads; the wrapper never mutates anything, and
// calls through the vtable are what the engine's own threads do.
//
// Exception: `GameEventManager` deliberately opts *out* of this blanket impl (a
// `PhantomData<*mut ()>` in the wrapper) — its listener registry is game-thread-only. See
// `crate::events::GameEventManager`.
unsafe impl<T> Send for Interface<T> {}
// SAFETY: shared reads only; same reasoning as `Send`.
unsafe impl<T> Sync for Interface<T> {}

/// Ask a module's `CreateInterface` factory for `version`, typed. `None` on any null path
/// (no factory, bad version string, unknown interface) — see [`Module::create_interface`].
#[must_use]
pub fn create<T>(module: &Module, version: &str) -> Option<Interface<T>> {
    let raw = module.create_interface(version)?;
    // SAFETY: transmuting the factory's untyped `void *` to the interface type the caller
    // named; the layout (a leading vtable pointer) is the generated binding's premise.
    let typed = unsafe { Interface::from_raw(raw.as_ptr().cast::<T>()) }?;
    Some(typed)
}
