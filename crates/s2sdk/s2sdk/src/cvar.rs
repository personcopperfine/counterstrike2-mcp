//! Convar access through the engine's `ICvar` (`VEngineCvar007`): look up registered
//! convars and read their metadata and current values, read-only.
//!
//! All calls go through the generated `ICvar_vtable` and the `ConVarData` /
//! `ConVarRef` records of `s2sdk_sys::generated::cvar` — no hand-written slots or offsets.
//! The value read follows upstream `ConVarData::Value` (`tier0_perproject/convar.cpp`):
//! `Value(slot) = &m_Values[GetDataByteSize() * slot]`; slot 0 starts at the inline
//! `m_Values` array, so reinterpreting it as `CVValue_t` gives the live value.

use std::ffi::CStr;
use std::ptr::NonNull;

use s2sdk_sys::generated as sys;

use crate::error::{Error, Result};
use crate::interface::{create, Interface};
use crate::module::{versions, Module};

/// The cvar system interface (`VEngineCvar007`).
///
/// Which library serves it is not pinned by the headers (live check pending):
/// [`Cvar::from_process`] tries `tier0` first, then `engine2` (per target:
/// `libtier0.so`/`libengine2.so` on Linux, `tier0.dll`/`engine2.dll` on Windows).
#[derive(Clone, Copy, Debug)]
pub struct Cvar(Interface<sys::ICvar>);

/// The libraries probed for [`versions::CVAR_INTERFACE_VERSION`], in probe order, spelled per
/// target through [`crate::module::names`]. Live-verified (WP6b): the Linux dedicated server's
/// `VEngineCvar007` comes from **`libtier0.so`** (the module also carries the version string;
/// `libengine2.so` does not), and [`Cvar::from_process_with_lib`] reports which one answered.
/// The Windows order mirrors it (`tier0.dll` first); the Windows side is type-checked only,
/// not live.
pub const CVAR_LIBS: [&str; 2] = [crate::module::names::TIER0, crate::module::names::ENGINE2];

impl Cvar {
    /// Create `VEngineCvar007` from an already-found module: `None` when this module does not
    /// serve the version. The per-library step of [`Cvar::from_process_with_lib`], public so a
    /// caller can drive its own probe order.
    #[must_use]
    pub fn from_module(module: &Module) -> Option<Self> {
        create(module, versions::CVAR_INTERFACE_VERSION).map(Self)
    }

    /// Create `VEngineCvar007`, probing [`CVAR_LIBS`] in order, and report which library
    /// served it. Non-blocking: at most one `find_loaded` probe per candidate plus one
    /// `CreateInterface` call each.
    ///
    /// # Errors
    ///
    /// [`Error::ModuleNotLoaded`] when none of [`CVAR_LIBS`] is mapped;
    /// [`Error::InterfaceMissing`] when one is mapped but none serves the version.
    pub fn from_process_with_lib() -> Result<(Self, &'static str)> {
        let mut module_missing = true;
        for lib in CVAR_LIBS {
            let Some(module) = Module::find_loaded(lib) else {
                continue;
            };
            module_missing = false;
            if let Some(cvar) = Self::from_module(&module) {
                return Ok((cvar, lib));
            }
        }
        if module_missing {
            Err(Error::ModuleNotLoaded(CVAR_LIBS.join(" / ")))
        } else {
            Err(Error::InterfaceMissing(
                versions::CVAR_INTERFACE_VERSION.to_string(),
            ))
        }
    }

    /// Create `VEngineCvar007`, probing [`CVAR_LIBS`] in order (per target) for the one with
    /// the factory. Non-blocking: two `find_loaded` probes at most, plus one
    /// `CreateInterface` call each.
    ///
    /// # Errors
    ///
    /// [`Error::ModuleNotLoaded`] when neither library is mapped;
    /// [`Error::InterfaceMissing`] when both are mapped but neither serves the version.
    pub fn from_process() -> Result<Self> {
        Self::from_process_with_lib().map(|(cvar, _lib)| cvar)
    }

    /// Wrap a raw `ICvar` pointer.
    ///
    /// # Safety
    ///
    /// `p` must point at a live `ICvar` (the engine's singleton); it must outlive the
    /// returned [`Cvar`] and every [`ConVar`] derived from it.
    #[must_use]
    pub unsafe fn from_raw(p: *mut sys::ICvar) -> Option<Self> {
        // SAFETY: the caller's contract is the wrapper's guarantee.
        unsafe { Interface::from_raw(p).map(Self) }
    }

    /// The raw interface pointer.
    #[must_use]
    pub fn as_ptr(&self) -> *mut sys::ICvar {
        self.0.as_ptr()
    }

    /// Look up a registered convar by name (`FindConVar(name, false)`).
    ///
    /// # Errors
    ///
    /// [`Error::MissingSlot`] when the slot has no typed signature;
    /// [`Error::NotFound`] when the ref's access index is invalid (upstream
    /// `ConVarRef::kInvalidAccessIndex`); [`Error::NullReturn`] when `GetConVarData` comes
    /// back null; [`Error::Utf8`] for a malformed name.
    pub fn find(&self, name: &str) -> Result<ConVar> {
        // SAFETY: the interface points at a live object (constructor contract).
        let vt = unsafe { sys::ICvar::vtable(self.0.as_ptr()) };
        let find = vt
            .FindConVar
            .ok_or(Error::MissingSlot("ICvar::FindConVar"))?;
        let cname = std::ffi::CString::new(name).map_err(|_| Error::Utf8)?;
        // SAFETY: the entry's generated signature. `FindConVar` returns the `ConVarRef` by
        // value: on Linux the binding spells that (8 bytes in RAX); on Windows MSVC returns
        // every class through a hidden pointer after `this`, which the binding spells as an
        // explicit `ret` parameter (see README rule 7), filled before the call returns.
        #[cfg(not(windows))]
        let r#ref = unsafe { find(self.0.as_ptr(), cname.as_ptr(), false) };
        #[cfg(windows)]
        let r#ref = {
            let mut out = std::mem::MaybeUninit::<sys::ConVarRef>::zeroed();
            // SAFETY: `out` is a valid, writable `ConVarRef` the callee initialises.
            unsafe { find(self.0.as_ptr(), out.as_mut_ptr(), cname.as_ptr(), false) };
            unsafe { out.assume_init() }
        };
        if r#ref.m_ConVarAccessIndex == ConVar::INVALID_ACCESS_INDEX {
            return Err(Error::NotFound(name.to_string()));
        }
        let get_data = vt
            .GetConVarData
            .ok_or(Error::MissingSlot("ICvar::GetConVarData"))?;
        // SAFETY: the entry's generated signature: `ConVarData *(this, ConVarRef)`.
        let data = unsafe { get_data(self.0.as_ptr(), r#ref) };
        let data = NonNull::new(data).ok_or(Error::NullReturn("ICvar::GetConVarData"))?;
        Ok(ConVar { r#ref, data })
    }
}

/// One registered convar: the value reference plus a pointer to its shared data record.
#[derive(Clone, Copy)]
pub struct ConVar {
    /// The ref the lookup returned (`FindConVar`), by value.
    pub r#ref: sys::ConVarRef,
    /// The convar's data record (`GetConVarData`).
    data: NonNull<sys::ConVarData>,
}

impl core::fmt::Debug for ConVar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `ConVarRef` has no generated Debug; print its two indices instead.
        f.debug_struct("ConVar")
            .field("access_index", &self.r#ref.m_ConVarAccessIndex)
            .field("registered_index", &self.r#ref.m_ConVarRegisteredIndex)
            .field("data", &self.data.as_ptr())
            .finish()
    }
}

impl ConVar {
    /// Upstream `ConVarRef::kInvalidAccessIndex` (`public/tier0/convar.h`): a ref with this
    /// access index is the "not found" value.
    pub const INVALID_ACCESS_INDEX: u16 = 0xFFFF;

    /// The shared data record, as a raw pointer.
    ///
    /// Callers read individual fields through [`core::ptr::addr_of!`] (`read()`): no Rust
    /// reference is ever formed over the engine-mutated record.
    fn data_ptr(&self) -> *const sys::ConVarData {
        self.data.as_ptr()
    }

    /// The convar's registered name (`m_pszName`).
    ///
    /// # Errors
    ///
    /// [`Error::NullReturn`], [`Error::Utf8`].
    pub fn name(&self) -> Result<String> {
        // SAFETY: live record (GetConVarData contract), raw field read only.
        unsafe {
            crate::schema::cstr_to_string(core::ptr::addr_of!((*self.data_ptr()).m_pszName).read())
        }
    }

    /// The convar's help string (`m_pszHelpString`).
    ///
    /// # Errors
    ///
    /// [`Error::NullReturn`], [`Error::Utf8`].
    pub fn help(&self) -> Result<String> {
        // SAFETY: live record, raw field read only.
        unsafe {
            crate::schema::cstr_to_string(
                core::ptr::addr_of!((*self.data_ptr()).m_pszHelpString).read(),
            )
        }
    }

    /// The convar's value type (`m_eVarType`).
    #[must_use]
    pub fn var_type(&self) -> sys::EConVarType {
        // SAFETY: live record, raw field read only.
        unsafe { core::ptr::addr_of!((*self.data_ptr()).m_eVarType).read() }
    }

    /// The convar's flags (`m_nFlags`).
    #[must_use]
    pub fn flags(&self) -> u64 {
        // SAFETY: live record, raw field read only.
        unsafe { core::ptr::addr_of!((*self.data_ptr()).m_nFlags).read() }
    }

    /// The convar's current value (slot 0 — server convars have one value per split-screen
    /// slot, and slot 0 is the live one for plugins).
    ///
    /// Upstream `ConVarData::Value` (`tier0_perproject/convar.cpp`) computes
    /// `&m_Values[GetDataByteSize() * slot]`; slot 0 is therefore the inline `m_Values`
    /// array itself, reinterpreted as `CVValue_t` — no hand-written offset.
    ///
    /// Scalar reads only: for a string convar this returns
    /// [`CvarValue::StringUnavailableOffThread`] instead of dereferencing the value's
    /// `CUtlString` — the game thread may overwrite/free it concurrently. Copy the string
    /// with [`ConVar::value_string_on_game_thread`] on the game thread.
    ///
    /// # Errors
    ///
    /// [`Error::Utf8`] never happens for the scalar paths remaining
    /// ([`CvarValue::StringUnavailableOffThread`] covers the string type).
    pub fn value(&self) -> Result<CvarValue> {
        // SAFETY: `m_Values` is `alignas(CVValue_t)` (checked by the generated layout
        // asserts) and holds at least one `CVValue_t`; the record is live (GetConVarData
        // contract) and read through `addr_of!` — no reference over engine memory.
        unsafe {
            let data = self.data_ptr();
            let var_type = core::ptr::addr_of!((*data).m_eVarType).read();
            let value_ptr = core::ptr::addr_of!((*data).m_Values).cast::<sys::CVValue_t>();
            let value = value_ptr.read();
            CvarValue::from_union(var_type, &value)
        }
    }

    /// Copy a string convar's value (slot 0).
    ///
    /// **Game-thread only, hence `unsafe`** — safety is a thread-contract, not a layout
    /// contract. `m_StringValue` is a `CUtlString` backed by memory the game thread owns:
    /// concurrent threads write and free it while we copy, and upstream exposes no
    /// synchronized read. On the game thread this is exactly what upstream's typed
    /// accessor does (`tier0_perproject/convar.cpp`), so the copy is race-free there.
    ///
    /// [`ConVar::value`] deliberately returns [`CvarValue::StringUnavailableOffThread`]
    /// for string convars instead of touching the pointer.
    ///
    /// # Errors
    ///
    /// [`Error::CvarNotString`] when `m_eVarType` is not `EConVarType_String`;
    /// [`Error::Utf8`] when the value is not valid UTF-8 (empty is fine).
    ///
    /// # Safety
    ///
    /// Must be called on the game thread (the thread that owns/updates the convar). The
    /// convar data record must still be live (the [`Cvar`] constructor contract).
    pub unsafe fn value_string_on_game_thread(&self) -> Result<String> {
        // SAFETY: live record (contract), raw field read only; the deref of the string
        // pointer is the game-thread-only part covered by the caller's safety contract.
        unsafe {
            let data = self.data_ptr();
            let var_type = core::ptr::addr_of!((*data).m_eVarType).read();
            if var_type != sys::EConVarType::EConVarType_String {
                return Err(Error::CvarNotString(var_type));
            }
            let value_ptr = core::ptr::addr_of!((*data).m_Values).cast::<sys::CVValue_t>();
            // Copy the bytes first: the union read happens before any use of the pointer.
            let value = value_ptr.read();
            let p = value.m_StringValue.m_pString;
            if p.is_null() {
                // A null string pointer means empty (upstream treats "" the same way).
                return Ok(String::new());
            }
            // SAFETY: `p` is non-null and NUL-terminated while the game thread holds it
            // (caller's contract: we are on the game thread).
            let s = CStr::from_ptr(p);
            Ok(s.to_str().map_err(|_| Error::Utf8)?.to_owned())
        }
    }
}

/// The current value of a convar, tagged by [`sys::EConVarType`].
///
/// Every variant carries the value read from the convar's slot-0 `CVValue_t`. Tags the
/// generated `EConVarType_*` consts do not cover (or genuinely untyped data) map to
/// [`CvarValue::Unknown`].
#[derive(Clone, Debug, PartialEq)]
pub enum CvarValue {
    /// `EConVarType_Bool`.
    Bool(bool),
    /// `EConVarType_Int16`.
    I16(i16),
    /// `EConVarType_UInt16`.
    U16(u16),
    /// `EConVarType_Int32`.
    I32(i32),
    /// `EConVarType_UInt32`.
    U32(u32),
    /// `EConVarType_Int64`.
    I64(i64),
    /// `EConVarType_UInt64`.
    U64(u64),
    /// `EConVarType_Float32`.
    F32(f32),
    /// `EConVarType_Float64`.
    F64(f64),
    /// `EConVarType_String`: a string convar's value **cannot** be copied off the game
    /// thread — `m_StringValue` is a `CUtlString` the game thread may overwrite/free
    /// concurrently, so dereferencing its pointer from another thread is a race. Use
    /// [`ConVar::value_string_on_game_thread`] on the game thread to copy it.
    StringUnavailableOffThread,
    /// `EConVarType_Color`.
    Color([u8; 4]),
    /// `EConVarType_Vector2`.
    Vec2([f32; 2]),
    /// `EConVarType_Vector3` (the game's `Vector`).
    Vec3([f32; 3]),
    /// `EConVarType_Vector4`.
    Vec4([f32; 4]),
    /// `EConVarType_Qangle`.
    QAngle([f32; 3]),
    /// Any tag the generated binding does not type (including `Invalid` and `VectorWS`,
    /// which has no exact Rust record).
    Unknown(sys::EConVarType),
}

impl CvarValue {
    /// Read the union member matching `var_type`. Unknown tags return
    /// [`CvarValue::Unknown`] without touching the union (reading an inactive member of
    /// the wrong shape is what we are avoiding for untyped tags).
    ///
    /// # Errors
    ///
    /// [`Error::Utf8`] when a string value is not valid UTF-8.
    ///
    /// # Safety
    ///
    /// `value` must point at a live, correctly aligned `CVValue_t` whose union member
    /// matching `var_type` is initialized (the convar data record's contract).
    unsafe fn from_union(var_type: sys::EConVarType, value: &sys::CVValue_t) -> Result<Self> {
        use sys::EConVarType as T;
        // SAFETY: each arm reads exactly the union member `m_eVarType` selects, per the
        // caller's contract.
        unsafe {
            match var_type {
                T::EConVarType_Bool => Ok(Self::Bool(value.m_bValue)),
                T::EConVarType_Int16 => Ok(Self::I16(value.m_i16Value)),
                T::EConVarType_UInt16 => Ok(Self::U16(value.m_u16Value)),
                T::EConVarType_Int32 => Ok(Self::I32(value.m_i32Value)),
                T::EConVarType_UInt32 => Ok(Self::U32(value.m_u32Value)),
                T::EConVarType_Int64 => Ok(Self::I64(value.m_i64Value)),
                T::EConVarType_UInt64 => Ok(Self::U64(value.m_u64Value)),
                T::EConVarType_Float32 => Ok(Self::F32(value.m_fl32Value)),
                T::EConVarType_Float64 => Ok(Self::F64(value.m_fl64Value)),
                T::EConVarType_String => {
                    // Not race-free off the game thread: the string is a `CUtlString` the
                    // game thread may free concurrently, so `value()` never dereferences
                    // it. Copy it with `ConVar::value_string_on_game_thread`.
                    Ok(Self::StringUnavailableOffThread)
                }
                T::EConVarType_Color => Ok(Self::Color(value.m_clrValue._color)),
                T::EConVarType_Vector2 => {
                    Ok(Self::Vec2([value.m_vec2Value.x, value.m_vec2Value.y]))
                }
                T::EConVarType_Vector3 => Ok(Self::Vec3([
                    value.m_vec3Value.x,
                    value.m_vec3Value.y,
                    value.m_vec3Value.z,
                ])),
                T::EConVarType_Vector4 => Ok(Self::Vec4([
                    value.m_vec4Value.x,
                    value.m_vec4Value.y,
                    value.m_vec4Value.z,
                    value.m_vec4Value.w,
                ])),
                T::EConVarType_Qangle => Ok(Self::QAngle([
                    value.m_angValue.x,
                    value.m_angValue.y,
                    value.m_angValue.z,
                ])),
                other => Ok(Self::Unknown(other)),
            }
        }
    }
}
