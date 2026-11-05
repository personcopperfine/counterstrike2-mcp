//! The schema system: type scopes, declared classes and their fields, read-only.
//!
//! All calls go through the generated `ISchemaSystem_vtable`,
//! `CSchemaSystemTypeScope_vtable` (whose `base` is `ISchemaSystemTypeScope`) and the
//! `SchemaClassInfoData_t` record layout of `s2sdk_sys::generated::schema` — no hand-written
//! slots or offsets.

use core::ffi::{c_char, CStr};
use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr;

use s2sdk_sys::generated as sys;

use crate::error::{Error, Result};
use crate::interface::{create, Interface};
use crate::module::{versions, Module};

/// Module name for [`SchemaSystem::type_scope`], the **server** module (per target:
/// `libserver.so` on Linux, `server.dll` on Windows — see [`crate::module::names`]).
/// **Live-verified on Linux (WP6b)**: `libserver.so` is the name the `libserver.so` type scope
/// is registered under, and the scope's own `name()` is the same string. It only resolves
/// *after* the server module has registered its schema bindings (~2 s into a dedicated-server
/// boot), and a lookup before that is indistinguishable from success: see
/// [`SchemaSystem::type_scope`] and [`TypeScope::is_global_scope`].
pub const SERVER_MODULE: &str = crate::module::names::SERVER;

/// Alternative module name for the server scope: the **extension-less** spelling
/// (`"server"`), which the engine resolves to the same bindings on some paths. Live-verified
/// (WP6b) **not** needed on the Linux dedicated server (`SERVER_MODULE` resolves); kept
/// because the engine's lookup never reports a miss, so trying more than one name is the only
/// way to tell "registered under another name" from "not registered yet". On Windows the
/// `GetModuleHandleW` lookup wants the `.dll` spelling ([`SERVER_MODULE`]), so this is an
/// extra miss there and only the `FindTypeScopeForModule` side can use it.
pub const SERVER_MODULE_ALT: &str = "server";

/// Convert a C string pointer into an owned `String`. `None` pointer maps to
/// [`Error::NullReturn`]; invalid UTF-8 maps to [`Error::Utf8`] (`to_string_lossy` is for
/// display only — names that reach stored state are validated).
pub(crate) fn cstr_to_string(p: *const c_char) -> Result<String> {
    if p.is_null() {
        return Err(Error::NullReturn("C string"));
    }
    // SAFETY: `p` is non-null and points at a NUL-terminated string owned by the callee for
    // the duration of the read (schema strings are static).
    let s = unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| Error::Utf8)?;
    Ok(s.to_owned())
}

/// Null-terminated Rust view of `name`; interior NUL bytes map to [`Error::Utf8`] (a C string
/// cannot carry them).
pub(crate) fn cstring_of(name: &str) -> Result<CString> {
    CString::new(name).map_err(|_| Error::Utf8)
}

/// The schema system interface (`SchemaSystem_001`, served by `libschemasystem.so`).
#[derive(Clone, Copy, Debug)]
pub struct SchemaSystem(Interface<sys::ISchemaSystem>);

impl SchemaSystem {
    /// Find the schema system module in this process and create `SchemaSystem_001`
    /// (per target: `libschemasystem.so` / `schemasystem.dll`; see [`crate::module::names`]).
    /// Non-blocking: one `find_loaded` probe + one `CreateInterface` call.
    ///
    /// # Errors
    ///
    /// [`Error::ModuleNotLoaded`] when the library is not mapped yet;
    /// [`Error::InterfaceMissing`] when the factory does not serve the version.
    pub fn from_process() -> Result<Self> {
        const LIB: &str = crate::module::names::SCHEMASYSTEM;
        let module =
            Module::find_loaded(LIB).ok_or_else(|| Error::ModuleNotLoaded(LIB.to_string()))?;
        let version = versions::SCHEMASYSTEM_INTERFACE_VERSION;
        let iface =
            create(&module, version).ok_or_else(|| Error::InterfaceMissing(version.to_string()))?;
        Ok(Self(iface))
    }

    /// Wrap a raw `ISchemaSystem` pointer.
    ///
    /// # Safety
    ///
    /// `p` must point at a live `ISchemaSystem` (the engine's singleton); the object must
    /// outlive the returned [`SchemaSystem`] and every [`TypeScope`] derived from it.
    #[must_use]
    pub unsafe fn from_raw(p: *mut sys::ISchemaSystem) -> Option<Self> {
        // SAFETY: the caller's contract is the wrapper's guarantee.
        unsafe { Interface::from_raw(p).map(Self) }
    }

    /// The raw interface pointer, for downstream callers that compose on top.
    #[must_use]
    pub fn as_ptr(&self) -> *mut sys::ISchemaSystem {
        self.0.as_ptr()
    }

    /// Is the schema system ready to answer queries? (`SchemaSystemIsReady`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`] when the generated binding has no typed signature for the slot.
    pub fn is_ready(&self) -> Result<bool> {
        // SAFETY: the interface points at a live object (constructor contract).
        let vt = unsafe { sys::ISchemaSystem::vtable(self.0.as_ptr()) };
        let f = vt
            .SchemaSystemIsReady
            .ok_or(Error::MissingSlot("ISchemaSystem::SchemaSystemIsReady"))?;
        // SAFETY: the entry's generated signature: `bool (this)`.
        Ok(unsafe { f(self.0.as_ptr()) })
    }

    /// The type scope of a game module, e.g. [`SERVER_MODULE`] (`FindTypeScopeForModule`).
    /// `ppszBindingName` is passed as null (upstream's default argument); the binding name is
    /// not needed for read-only queries.
    ///
    /// **This call cannot report "the module has no scope yet"**: live-verified (WP6b), the
    /// engine's `FindTypeScopeForModule` returns the *global* scope (upstream's
    /// `GlobalTypeScope()`, named `"!GlobalTypes"`) on a miss instead of null. A caller that
    /// must tell the module's own scope from "not registered yet" has to ask
    /// [`TypeScope::is_global_scope`] (or poll until it is false).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`] when the slot has no typed signature; [`Error::NotFound`] only
    /// when the name is malformed (the engine never returns null for a valid name).
    pub fn type_scope(&self, module: &str) -> Result<TypeScope> {
        // SAFETY: the interface points at a live object (constructor contract).
        let vt = unsafe { sys::ISchemaSystem::vtable(self.0.as_ptr()) };
        let f = vt
            .FindTypeScopeForModule
            .ok_or(Error::MissingSlot("ISchemaSystem::FindTypeScopeForModule"))?;
        let cname = cstring_of(module)?;
        // SAFETY: the entry's generated signature; the binding-name out-pointer may be null
        // (upstream's `= nullptr` default).
        let p = unsafe { f(self.0.as_ptr(), cname.as_ptr(), ptr::null_mut()) };
        let scope = core::ptr::NonNull::new(p)
            .ok_or_else(|| Error::NotFound(format!("type scope for module {module:?}")))?;
        Ok(TypeScope(Interface { ptr: scope }))
    }

    /// The global type scope (`GlobalTypeScope`), which holds the shared builtin types.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`] when the slot has no typed signature; [`Error::NullReturn`]
    /// when the engine has no global scope (not initialized yet).
    pub fn global_type_scope(&self) -> Result<TypeScope> {
        // SAFETY: the interface points at a live object (constructor contract).
        let vt = unsafe { sys::ISchemaSystem::vtable(self.0.as_ptr()) };
        let f = vt
            .GlobalTypeScope
            .ok_or(Error::MissingSlot("ISchemaSystem::GlobalTypeScope"))?;
        // SAFETY: the entry's generated signature: `ptr (this)`.
        let p = unsafe { f(self.0.as_ptr()) };
        let scope = core::ptr::NonNull::new(p).ok_or(Error::NullReturn("GlobalTypeScope"))?;
        Ok(TypeScope(Interface { ptr: scope }))
    }
}

/// One type scope (`CSchemaSystemTypeScope`): the declared classes of one game module plus
/// the builtin types.
///
/// Calls go through `CSchemaSystemTypeScope_vtable.base` — the object is a
/// `CSchemaSystemTypeScope`, whose only base is `ISchemaSystemTypeScope` where the virtuals
/// live.
#[derive(Clone, Copy, Debug)]
pub struct TypeScope(Interface<sys::CSchemaSystemTypeScope>);

impl TypeScope {
    /// Wrap a raw `CSchemaSystemTypeScope` pointer.
    ///
    /// # Safety
    ///
    /// `p` must point at a live scope (the engine's own object); it must outlive the returned
    /// [`TypeScope`] and every [`ClassInfo`] derived from it.
    #[must_use]
    pub unsafe fn from_raw(p: *mut sys::CSchemaSystemTypeScope) -> Option<Self> {
        // SAFETY: the caller's contract is the wrapper's guarantee.
        unsafe { Interface::from_raw(p).map(Self) }
    }

    /// The raw interface pointer.
    #[must_use]
    pub fn as_ptr(&self) -> *mut sys::CSchemaSystemTypeScope {
        self.0.as_ptr()
    }

    /// `this` as the base interface type the generated `base`-vtable entries take (single
    /// inheritance: the address point of `CSchemaSystemTypeScope` is the base's).
    fn this(&self) -> *mut sys::ISchemaSystemTypeScope {
        self.0.as_ptr().cast::<sys::ISchemaSystemTypeScope>()
    }

    /// The scope's name (`GetScopeName`), e.g. the module name it was registered for.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::NullReturn`], [`Error::Utf8`].
    pub fn name(&self) -> Result<String> {
        // SAFETY: the scope points at a live object (constructor contract).
        let vt = unsafe { sys::CSchemaSystemTypeScope::vtable(self.0.as_ptr()) };
        let f = vt
            .base
            .GetScopeName
            .ok_or(Error::MissingSlot("ISchemaSystemTypeScope::GetScopeName"))?;
        // SAFETY: the entry's generated signature: `const char *(this)`.
        let p = unsafe { f(self.this()) };
        cstr_to_string(p)
    }

    /// Is this the engine's global type scope (`IsGlobalScope`)? The global scope is the one
    /// named `"!GlobalTypes"`.
    ///
    /// [`SchemaSystem::type_scope`] cannot answer "is this module registered": the engine's
    /// `FindTypeScopeForModule` returns the *global* scope on a miss instead of null, so a
    /// caller that must tell the server's own scope from "server not registered yet" has to
    /// ask here.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`] when the generated binding has no typed signature for the slot.
    pub fn is_global_scope(&self) -> Result<bool> {
        // SAFETY: the scope points at a live object (constructor contract).
        let vt = unsafe { sys::CSchemaSystemTypeScope::vtable(self.0.as_ptr()) };
        let f = vt
            .base
            .IsGlobalScope
            .ok_or(Error::MissingSlot("ISchemaSystemTypeScope::IsGlobalScope"))?;
        // SAFETY: the entry's generated signature: `bool (this)`.
        Ok(unsafe { f(self.this()) })
    }

    /// A declared class by name (`FindDeclaredClass`).
    ///
    /// **Game-thread only.** Upstream's `FindDeclaredClass` returns a
    /// `SchemaMetaInfoHandle_t` and can lazily create/promote the declared-class handle it
    /// looks up — that is engine mutation. For a call that is safe from another thread,
    /// use [`TypeScope::find_class_threadsafe`]; for a call that never mutates at all,
    /// [`TypeScope::find_raw_class_binding`].
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::NotFound`] when no class with that name is declared
    /// in this scope, [`Error::Utf8`] for a malformed name.
    pub fn find_class(&self, name: &str) -> Result<ClassInfo> {
        // SAFETY: the scope points at a live object (constructor contract).
        let vt = unsafe { sys::CSchemaSystemTypeScope::vtable(self.0.as_ptr()) };
        let f = vt.base.FindDeclaredClass.ok_or(Error::MissingSlot(
            "ISchemaSystemTypeScope::FindDeclaredClass",
        ))?;
        let cname = cstring_of(name)?;
        // SAFETY: the entry's generated signature: `SchemaMetaInfoHandle_t<CSchemaClassInfo>
        // (this, pszClassName)`, a one-pointer class. On Windows MSVC returns it through a
        // hidden pointer after `this`, which the binding spells as an explicit `ret` parameter.
        #[cfg(not(windows))]
        let p = unsafe { f(self.this(), cname.as_ptr()) };
        #[cfg(windows)]
        let p = {
            let mut out: *mut sys::CSchemaClassInfo = core::ptr::null_mut();
            // SAFETY: `out` is a valid, writable pointer slot the callee fills.
            unsafe { f(self.this(), &mut out, cname.as_ptr()) };
            out
        };
        let class = core::ptr::NonNull::new(p).ok_or_else(|| Error::NotFound(name.to_string()))?;
        Ok(ClassInfo { ptr: class })
    }

    /// A declared class by name, through `ResolveClassInfoThreadsafe` — the thread-safe
    /// variant upstream provides exactly for callers that are **not** the game thread
    /// (`public/schemasystem/minschema.h`): the scope may concurrently resolve and cache
    /// the class, and this virtual returns the result through a pointer out-parameter
    /// instead of mutating under the caller.
    ///
    /// [`TypeScope::find_class`] (`FindDeclaredClass`) stays the game-thread call. Note the
    /// mock tests exercise both paths; the probe (WP6b/7) reports whether the live vtable
    /// actually fills the threadsafe slot.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::NotFound`], [`Error::Utf8`].
    pub fn find_class_threadsafe(&self, name: &str) -> Result<ClassInfo> {
        // SAFETY: the scope points at a live object (constructor contract).
        let vt = unsafe { sys::CSchemaSystemTypeScope::vtable(self.0.as_ptr()) };
        let f = vt
            .base
            .ResolveClassInfoThreadsafe
            .ok_or(Error::MissingSlot(
                "ISchemaSystemTypeScope::ResolveClassInfoThreadsafe",
            ))?;
        let cname = cstring_of(name)?;
        let mut out: *const sys::CSchemaClassInfo = core::ptr::null();
        // SAFETY: the entry's generated signature:
        // `void (this, const CSchemaClassInfo **ppClassInfo, pszClassName)`; the
        // out-parameter and the name outlive the call.
        unsafe { f(self.this(), &mut out, cname.as_ptr()) };
        let class = core::ptr::NonNull::new(out.cast_mut())
            .ok_or_else(|| Error::NotFound(name.to_string()))?;
        Ok(ClassInfo { ptr: class })
    }

    /// The raw (unpromoted) class binding (`FindRawClassBinding`, overload 0 — the
    /// name-based one; `FindRawClassBinding_1` is the id-based overload).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`], [`Error::NotFound`], [`Error::Utf8`].
    pub fn find_raw_class_binding(&self, name: &str) -> Result<ClassInfo> {
        // SAFETY: the scope points at a live object (constructor contract).
        let vt = unsafe { sys::CSchemaSystemTypeScope::vtable(self.0.as_ptr()) };
        let f = vt.base.FindRawClassBinding.ok_or(Error::MissingSlot(
            "ISchemaSystemTypeScope::FindRawClassBinding",
        ))?;
        let cname = cstring_of(name)?;
        // SAFETY: the entry's generated signature: `CSchemaClassInfo *(this, pszClassName)`.
        let p = unsafe { f(self.this(), cname.as_ptr()) };
        let class = core::ptr::NonNull::new(p).ok_or_else(|| Error::NotFound(name.to_string()))?;
        Ok(ClassInfo { ptr: class })
    }
}

/// One declared schema class (`CSchemaClassInfo`), read-only.
///
/// All accessors read through the generated `SchemaClassInfoData_t` record layout
/// (`(*ptr).base` in C++ terms) — no hand-written offsets.
#[derive(Clone, Copy, Debug)]
pub struct ClassInfo {
    ptr: core::ptr::NonNull<sys::CSchemaClassInfo>,
}

impl ClassInfo {
    /// The raw pointer.
    #[must_use]
    pub fn as_ptr(&self) -> *mut sys::CSchemaClassInfo {
        self.ptr.as_ptr()
    }

    /// The `SchemaClassInfoData_t` base of the object, as a raw pointer.
    ///
    /// Callers read individual fields through [`core::ptr::addr_of!`] (`read()`): no Rust
    /// reference is ever formed over the engine-mutated record.
    fn data_ptr(&self) -> *const sys::SchemaClassInfoData_t {
        let p = self.ptr.as_ptr();
        // SAFETY: `p` is non-null (constructor contract); `addr_of!` only projects the
        // `base` field, it does not dereference or create a reference.
        unsafe { core::ptr::addr_of!((*p).base) }
    }

    /// The class's name (`m_pName`).
    ///
    /// # Errors
    ///
    /// [`Error::NullReturn`], [`Error::Utf8`].
    pub fn name(&self) -> Result<String> {
        // SAFETY: the class info is live per the constructor contract; the pointer field is
        // only read.
        let psz_name = unsafe { core::ptr::addr_of!((*self.data_ptr()).m_pName).read() };
        cstr_to_string(psz_name)
    }

    /// The project (module) the class was declared in (`m_pProjectName`).
    ///
    /// # Errors
    ///
    /// [`Error::NullReturn`], [`Error::Utf8`].
    pub fn project_name(&self) -> Result<String> {
        // SAFETY: live record, read only.
        let psz_project =
            unsafe { core::ptr::addr_of!((*self.data_ptr()).m_pProjectName).read() };
        cstr_to_string(psz_project)
    }

    /// The class's total size in bytes (`m_nSizeOf`).
    #[must_use]
    pub fn size(&self) -> i32 {
        // SAFETY: live record, read only.
        unsafe { core::ptr::addr_of!((*self.data_ptr()).m_nSizeOf).read() }
    }

    /// The number of declared fields (`m_nNumFields`).
    #[must_use]
    pub fn field_count(&self) -> u16 {
        // SAFETY: live record, read only.
        unsafe { core::ptr::addr_of!((*self.data_ptr()).m_nNumFields).read() }
    }

    /// An iterator over the declared fields (`m_pFields[0..m_nNumFields]`).
    #[must_use]
    pub fn fields(&self) -> Fields<'_> {
        let d = self.data_ptr();
        // SAFETY: live record; both fields are read as raw values, never references.
        unsafe {
            let fields = core::ptr::addr_of!((*d).m_pFields).read();
            let count = core::ptr::addr_of!((*d).m_nNumFields).read();
            Fields {
                ptr: fields.cast_const(),
                remaining: count,
                _marker: PhantomData,
            }
        }
    }

    /// The direct base classes (`m_pBaseClasses[0..m_nNumBaseClasses]`), each with its
    /// inheritance offset. One level only: walk the returned [`ClassInfo`] values to climb.
    #[must_use]
    pub fn base_classes(&self) -> Vec<(u32, ClassInfo)> {
        let d = self.data_ptr();
        // SAFETY: live record; raw field reads only (see `data_ptr`).
        unsafe {
            let count = usize::from(core::ptr::addr_of!((*d).m_nNumBaseClasses).read());
            let base_classes = core::ptr::addr_of!((*d).m_pBaseClasses).read();
            if base_classes.is_null() {
                return Vec::new();
            }
            (0..count)
                .filter_map(|i| {
                    // SAFETY: `i` is within `m_nNumBaseClasses` of a live array, per the
                    // class info's own counters.
                    let base = base_classes.add(i).read();
                    core::ptr::NonNull::new(base.m_pClass)
                        .map(|ptr| (base.m_nOffset, ClassInfo { ptr }))
                })
                .collect()
        }
    }
}

/// Iterator over a [`ClassInfo`]'s declared fields.
///
/// Yields [`Result`]s because field names and type names are C strings that may fail the
/// UTF-8 check ([`Error::Utf8`]) or be null ([`Error::NullReturn`]).
pub struct Fields<'a> {
    ptr: *const sys::SchemaClassFieldData_t,
    remaining: u16,
    _marker: PhantomData<&'a ClassInfo>,
}

impl Iterator for Fields<'_> {
    type Item = Result<Field>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        // SAFETY: the array has `m_nNumFields` live entries (class info contract); the
        // counter above bounds every `add`.
        let raw = unsafe { self.ptr.read() };
        // SAFETY: advance one array element.
        unsafe {
            self.ptr = self.ptr.add(1);
        }
        Some(Field::from_raw(&raw))
    }
}

impl core::fmt::Debug for Fields<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fields")
            .field("remaining", &self.remaining)
            .finish()
    }
}

/// One declared field of a schema class, read-only.
#[derive(Clone, Debug)]
pub struct Field {
    /// Field name (`m_pName`).
    pub name: String,
    /// Field offset within the class (`m_nSingleInheritanceOffset`).
    pub offset: i32,
    /// The field type's name (`m_pType->m_sTypeName.m_pString`), e.g. `"int32"` or
    /// `"CHandle< CBaseEntity >"`.
    pub type_name: String,
    /// The type's category (`m_pType->m_eTypeCategory`).
    pub category: sys::SchemaTypeCategory_t,
    /// The type's atomic category (`m_pType->m_eAtomicCategory`).
    pub atomic_category: sys::SchemaAtomicCategory_t,
}

impl Field {
    /// Read one field descriptor from its raw generated record. The type-descriptor fields
    /// are read through [`core::ptr::addr_of!`] (`read()`): `m_pType` points into
    /// engine-mutated memory, so no Rust reference is ever formed over it.
    ///
    /// # Errors
    ///
    /// [`Error::NullReturn`] when `m_pType` is null or `m_pName` is null;
    /// [`Error::Utf8`] for a non-UTF-8 name.
    fn from_raw(raw: &sys::SchemaClassFieldData_t) -> Result<Field> {
        let name = cstr_to_string(raw.m_pName)?;
        let schema_type = core::ptr::NonNull::new(raw.m_pType)
            .ok_or(Error::NullReturn("SchemaClassFieldData_t::m_pType"))?;
        // SAFETY: the type descriptor is live per the class info contract; each field is
        // read through `addr_of!` as a raw value — no reference over engine memory.
        unsafe {
            let t = schema_type.as_ptr();
            let type_name = cstr_to_string(core::ptr::addr_of!((*t).m_sTypeName.m_pString).read())?;
            Ok(Field {
                name,
                offset: raw.m_nSingleInheritanceOffset,
                type_name,
                category: core::ptr::addr_of!((*t).m_eTypeCategory).read(),
                atomic_category: core::ptr::addr_of!((*t).m_eAtomicCategory).read(),
            })
        }
    }
}
