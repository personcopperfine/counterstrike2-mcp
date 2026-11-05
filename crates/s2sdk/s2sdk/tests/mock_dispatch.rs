//! Mock-vtable dispatch tests (Linux only): each mock is a `#[repr(C)]` struct whose vtable
//! is built from the generated `X_vtable` struct with `..X_vtable::NULL`, so a wrong slot
//! means the call lands on `None` -> `Error::MissingSlot` -> test failure.
//!
//! Not unit-testable (WP6b/7 probe, live): CreateInterface strings, which lib serves
//! `VEngineCvar007`, `"libserver.so"` vs `"server"`, `GameEventsManager002` availability,
//! and that `AddListener` fires on the game thread.

#![cfg(not(windows))]

use core::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, OnceLock};

use s2sdk::cvar::{Cvar, CvarValue, CVAR_LIBS};
use s2sdk::error::Error;
use s2sdk::events::{GameEventManager, KeySymbol, Listener};
use s2sdk::module::Module;
use s2sdk::schema::SchemaSystem;
use s2sdk_sys::generated as sys;

/// Wrapper for mock objects held in `static`s: a `static` requires `Sync`, and the mocks are
/// plain C-layout records full of raw pointers. Test mocks are never actually shared across
/// threads (the engine is not running here), so blanket-`Sync` is honest for this file.
#[repr(transparent)]
struct Mock<T>(T);
unsafe impl<T> Sync for Mock<T> {}
// `OnceLock<Mock<_>>` statics (the lazily-built mock records) also need `Send`; same
// reasoning: test mocks are never actually shared across threads.
unsafe impl<T> Send for Mock<T> {}

// ---------------------------------------------------------------------------
// schema_dispatch + schema_slot_index mocks
// ---------------------------------------------------------------------------

/// The mock `CSchemaType` the fields point at: a builtin with type name "int32".
static MOCK_TYPE_INT32: Mock<sys::CSchemaType> = Mock(sys::CSchemaType {
    vtable_: core::ptr::null(),
    m_sTypeName: sys::CUtlString {
        m_pString: c"int32".as_ptr().cast_mut(),
    },
    m_pTypeScope: core::ptr::null_mut(),
    m_eTypeCategory: sys::SchemaTypeCategory_t::SCHEMA_TYPE_BUILTIN,
    m_eAtomicCategory: sys::SchemaAtomicCategory_t::SCHEMA_ATOMIC_PLAIN,
    _pad5: [0; 6],
});

/// The mock `CSchemaType` for a class-typed field ("CBaseEntity" style).
static MOCK_TYPE_CLASS: Mock<sys::CSchemaType> = Mock(sys::CSchemaType {
    vtable_: core::ptr::null(),
    m_sTypeName: sys::CUtlString {
        m_pString: c"CBaseEntity".as_ptr().cast_mut(),
    },
    m_pTypeScope: core::ptr::null_mut(),
    m_eTypeCategory: sys::SchemaTypeCategory_t::SCHEMA_TYPE_DECLARED_CLASS,
    m_eAtomicCategory: sys::SchemaAtomicCategory_t::SCHEMA_ATOMIC_PLAIN,
    _pad5: [0; 6],
});

/// Two field descriptors the mock class info points at.
static MOCK_FIELDS: Mock<[sys::SchemaClassFieldData_t; 2]> = Mock([
    sys::SchemaClassFieldData_t {
        m_pName: c"m_nHealth".as_ptr(),
        m_pType: (&raw const MOCK_TYPE_INT32.0).cast_mut(),
        m_nSingleInheritanceOffset: 12,
        m_nNumMetadata: 0,
        _pad4: [0; 2],
        m_pMetadata: core::ptr::null_mut(),
    },
    sys::SchemaClassFieldData_t {
        m_pName: c"m_pOwner".as_ptr(),
        m_pType: (&raw const MOCK_TYPE_CLASS.0).cast_mut(),
        m_nSingleInheritanceOffset: 40,
        m_nNumMetadata: 0,
        _pad4: [0; 2],
        m_pMetadata: core::ptr::null_mut(),
    },
]);

/// The mock declared class: a real `CSchemaClassInfo` value with two fields.
static MOCK_CLASS_INFO: Mock<sys::CSchemaClassInfo> = Mock(sys::CSchemaClassInfo {
    base: sys::SchemaClassInfoData_t {
        m_pPrimaryBinding: core::ptr::null_mut(),
        m_pName: c"CBaseEntity".as_ptr(),
        m_pProjectName: c"server".as_ptr(),
        m_pszCPPName: c"CBaseEntity".as_ptr(),
        m_nSizeOf: 1234,
        m_nNumFields: 2,
        m_nNumMetadata: 0,
        m_nAlignOf: 8,
        m_nNumBaseClasses: 0,
        m_nMultipleInheritanceDepth: 0,
        m_nSingleInheritanceDepth: 0,
        _pad11: [0; 2],
        m_pFields: core::ptr::addr_of!(MOCK_FIELDS.0).cast_mut().cast(),
        m_pBaseClasses: core::ptr::null_mut(),
        m_pDataDesc: core::ptr::null_mut(),
        m_pMetadata: core::ptr::null_mut(),
        m_pOuterTypeScope: core::ptr::null_mut(),
        m_pTypeOf: core::ptr::null_mut(),
        m_nClassFlags1: 0,
        m_nClassFlags2: 0,
        m_pClassOpsFn: core::ptr::null(),
    },
});

/// The mock type scope's inner vtable: `base.GetScopeName` / `base.FindDeclaredClass` /
/// `base.FindRawClassBinding` / `base.ResolveClassInfoThreadsafe` filled, everything else
/// null (`..ISchemaSystemTypeScope_vtable::NULL`).
static MOCK_SCOPE_VTABLE: Mock<sys::CSchemaSystemTypeScope_vtable> =
    Mock(sys::CSchemaSystemTypeScope_vtable {
        base: sys::ISchemaSystemTypeScope_vtable {
            GetScopeName: Some(mock_get_scope_name),
            IsGlobalScope: Some(mock_is_global_scope),
            FindDeclaredClass: Some(mock_find_declared_class),
            FindRawClassBinding: Some(mock_find_declared_class),
            ResolveClassInfoThreadsafe: Some(mock_resolve_class_info_threadsafe),
            ..sys::ISchemaSystemTypeScope_vtable::NULL
        },
    });

/// The mock type scope object: vtable pointer only (that is all the generated binding
/// touches).
static MOCK_TYPE_SCOPE: Mock<sys::CSchemaSystemTypeScope> = Mock(sys::CSchemaSystemTypeScope {
    vtable: &raw const MOCK_SCOPE_VTABLE.0,
});

/// `GetScopeName` mock: the scope is named like the server module.
unsafe extern "C" fn mock_get_scope_name(_this: *mut sys::ISchemaSystemTypeScope) -> *const c_char {
    c"libserver.so".as_ptr()
}

/// `IsGlobalScope` mock: the mock scope is a module scope, not the global one. The mock vtable
/// selects between the two with [`MOCK_SCOPE_IS_GLOBAL`] so the `is_global_scope` dispatch and
/// the probe's "a miss returns the global scope" rule can both be exercised.
static MOCK_SCOPE_IS_GLOBAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

unsafe extern "C" fn mock_is_global_scope(_this: *mut sys::ISchemaSystemTypeScope) -> bool {
    MOCK_SCOPE_IS_GLOBAL.load(std::sync::atomic::Ordering::SeqCst)
}

/// `FindDeclaredClass` mock: every name resolves to the static mock class info.
unsafe extern "C" fn mock_find_declared_class(
    _this: *mut sys::ISchemaSystemTypeScope,
    _psz_class_name: *const c_char,
) -> *mut sys::CSchemaClassInfo {
    (&raw const MOCK_CLASS_INFO.0).cast_mut()
}

/// `ResolveClassInfoThreadsafe` mock: the same class info, through the out-parameter form
/// (`void (this, const CSchemaClassInfo **ppClassInfo, pszClassName)`).
unsafe extern "C" fn mock_resolve_class_info_threadsafe(
    _this: *mut sys::ISchemaSystemTypeScope,
    pp_class_info: *mut *const sys::CSchemaClassInfo,
    _psz_class_name: *const c_char,
) {
    if !pp_class_info.is_null() {
        *pp_class_info = &raw const MOCK_CLASS_INFO.0;
    }
}

/// The mock schema system's inner vtable: only `FindTypeScopeForModule` is filled.
static MOCK_SCHEMA_VTABLE: Mock<sys::ISchemaSystem_vtable> = Mock(sys::ISchemaSystem_vtable {
    FindTypeScopeForModule: Some(mock_find_type_scope_for_module),
    ..sys::ISchemaSystem_vtable::NULL
});

/// The module string the mock `FindTypeScopeForModule` saw last.
static LAST_MODULE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `FindTypeScopeForModule` mock: record the module string, return the mock scope.
unsafe extern "C" fn mock_find_type_scope_for_module(
    _this: *mut sys::ISchemaSystem,
    psz_module_name: *const c_char,
    _ppsz_binding_name: *mut *const c_char,
) -> *mut sys::CSchemaSystemTypeScope {
    if !psz_module_name.is_null() {
        // SAFETY: the caller passes a NUL-terminated module name for the call.
        let name = unsafe { CStr::from_ptr(psz_module_name) }
            .to_string_lossy()
            .into_owned();
        // The mock is single-threaded, but a poisoned lock would panic elsewhere — swallow
        // it and keep recording.
        if let Ok(mut q) = LAST_MODULE.lock() {
            q.push(name);
        }
    }
    (&raw const MOCK_TYPE_SCOPE.0).cast_mut()
}

/// The mock schema system object.
static MOCK_SCHEMA_SYSTEM: Mock<sys::ISchemaSystem> = Mock(sys::ISchemaSystem {
    vtable: &raw const MOCK_SCHEMA_VTABLE.0,
});

/// Drive a `SchemaSystem` through the struct-field vtable (`MOCK_SCHEMA_SYSTEM`).
fn schema_system_struct_field() -> SchemaSystem {
    // SAFETY: the mock object is live and its vtable is the generated layout.
    unsafe { SchemaSystem::from_raw((&raw const MOCK_SCHEMA_SYSTEM.0).cast_mut()) }
        .expect("mock object is non-null")
}

/// Drive a `SchemaSystem` through a slot-index vtable (test 2): a raw `[*const c_void; N]`
/// with the function pointer placed by `ISchemaSystem_slots::FindTypeScopeForModule`.
fn schema_system_slot_index() -> SchemaSystem {
    type FindFn = unsafe extern "C" fn(
        *mut sys::ISchemaSystem,
        *const c_char,
        *mut *const c_char,
    ) -> *mut sys::CSchemaSystemTypeScope;
    let f: FindFn = mock_find_type_scope_for_module;

    // The raw table: zeros everywhere except the slot the const names. The table length
    // comes from the `abi` calculator over the generated `VClass` static (no hand-written
    // length).
    static RAW_TABLE: OnceLock<Vec<usize>> = OnceLock::new();
    let table = RAW_TABLE.get_or_init(|| {
        let len =
            s2sdk_sys::abi::vtable_len(s2sdk_sys::abi::Abi::Itanium, &sys::ISchemaSystem_VCLASS);
        let mut table = vec![0usize; len];
        let slot = sys::ISchemaSystem_slots::FindTypeScopeForModule;
        assert!(
            slot < len,
            "slot const must fall inside the computed vtable length"
        );
        // Transmute the function pointer to a raw table entry (same bit pattern).
        let entry: usize = f as usize;
        table[slot] = entry;
        table
    });

    // Reinterpret the table as the generated vtable struct: if struct-field layout and
    // slot-index layout disagreed, the call would land on null -> MissingSlot -> fail.
    let vtable = table
        .as_slice()
        .as_ptr()
        .cast::<*const c_void>()
        .cast::<sys::ISchemaSystem_vtable>();
    // The mock object itself must be program-lifetime: a `static` (initialised via the
    // existing OnceLock pattern), not a stack local that would dangle on return.
    static OBJECT: OnceLock<Mock<sys::ISchemaSystem>> = OnceLock::new();
    let object = OBJECT.get_or_init(|| Mock(sys::ISchemaSystem { vtable }));
    // SAFETY: the mock object is program-lifetime (the `OBJECT` static above) and its
    // vtable pointer is valid for the program's life (`RAW_TABLE` never changes again).
    unsafe { SchemaSystem::from_raw(core::ptr::from_ref(&object.0).cast_mut()) }
        .expect("mock object is non-null")
}

fn mock_type_scope() -> s2sdk::schema::TypeScope {
    schema_system_struct_field()
        .type_scope("libserver.so")
        .expect("the mock resolves every module name")
}

#[test]
fn schema_dispatch_fields_round_trip() {
    let scope = mock_type_scope();
    assert_eq!(scope.name().unwrap(), "libserver.so");
    let class = scope
        .find_class("CBaseEntity")
        .expect("mock class resolves");
    assert_eq!(class.name().unwrap(), "CBaseEntity");
    assert_eq!(class.project_name().unwrap(), "server");
    assert_eq!(class.size(), 1234);
    assert_eq!(class.field_count(), 2);
    assert_eq!(class.base_classes().len(), 0);

    let fields: Vec<_> = class.fields().collect::<Result<_, _>>().unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "m_nHealth");
    assert_eq!(fields[0].offset, 12);
    assert_eq!(fields[0].type_name, "int32");
    assert_eq!(
        fields[0].category,
        sys::SchemaTypeCategory_t::SCHEMA_TYPE_BUILTIN
    );
    assert_eq!(
        fields[0].atomic_category,
        sys::SchemaAtomicCategory_t::SCHEMA_ATOMIC_PLAIN
    );
    assert_eq!(fields[1].name, "m_pOwner");
    assert_eq!(fields[1].offset, 40);
    assert_eq!(fields[1].type_name, "CBaseEntity");
    assert_eq!(
        fields[1].category,
        sys::SchemaTypeCategory_t::SCHEMA_TYPE_DECLARED_CLASS
    );

    // The recorded module string reached the mock with the name we passed.
    let last = LAST_MODULE.lock().unwrap();
    assert!(
        last.contains(&"libserver.so".to_string()),
        "recorded: {last:?}"
    );
}

#[test]
fn schema_slot_index_agrees_with_struct_field() {
    let scope = schema_system_slot_index()
        .type_scope("libserver.so")
        .expect("the mock resolves every module name");
    let class = scope
        .find_class("CBaseEntity")
        .expect("mock class resolves");
    let fields: Vec<_> = class.fields().collect::<Result<_, _>>().unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "m_nHealth");
    assert_eq!(fields[0].offset, 12);
    assert_eq!(fields[1].type_name, "CBaseEntity");
}

/// The scope's `IsGlobalScope` (added for the WP6b probe's `schema_scope` check): the engine's
/// `FindTypeScopeForModule` hands back the *global* scope on a miss, so the caller needs this
/// to tell "the server's own scope" from "not registered".
#[test]
fn schema_scope_is_global_scope_dispatches() {
    let scope = mock_type_scope();
    MOCK_SCOPE_IS_GLOBAL.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(!scope.is_global_scope().unwrap());
    MOCK_SCOPE_IS_GLOBAL.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(scope.is_global_scope().unwrap());
    MOCK_SCOPE_IS_GLOBAL.store(false, std::sync::atomic::Ordering::SeqCst);

    // A vtable without the slot maps to MissingSlot, never to a wrong answer.
    static NO_GLOBAL_VTABLE: Mock<sys::CSchemaSystemTypeScope_vtable> =
        Mock(sys::CSchemaSystemTypeScope_vtable {
            base: sys::ISchemaSystemTypeScope_vtable {
                GetScopeName: Some(mock_get_scope_name),
                ..sys::ISchemaSystemTypeScope_vtable::NULL
            },
        });
    static NO_GLOBAL_SCOPE: Mock<sys::CSchemaSystemTypeScope> = Mock(sys::CSchemaSystemTypeScope {
        vtable: &raw const NO_GLOBAL_VTABLE.0,
    });
    // SAFETY: the mock object is live and its vtable is the generated layout.
    let scope =
        unsafe { s2sdk::schema::TypeScope::from_raw((&raw const NO_GLOBAL_SCOPE.0).cast_mut()) }
            .expect("mock object is non-null");
    assert!(matches!(
        scope.is_global_scope(),
        Err(Error::MissingSlot("ISchemaSystemTypeScope::IsGlobalScope"))
    ));
}

#[test]
fn schema_find_raw_class_binding_works_too() {
    let scope = mock_type_scope();
    let class = scope
        .find_raw_class_binding("CBaseEntity")
        .expect("the mock FindRawClassBinding resolves");
    assert_eq!(class.name().unwrap(), "CBaseEntity");
    assert_eq!(class.field_count(), 2);
}

#[test]
fn schema_find_class_threadsafe_resolves() {
    let scope = mock_type_scope();
    let class = scope
        .find_class_threadsafe("CBaseEntity")
        .expect("the mock ResolveClassInfoThreadsafe resolves");
    assert_eq!(class.name().unwrap(), "CBaseEntity");
    assert_eq!(class.field_count(), 2);
    assert_eq!(class.size(), 1234);
}

#[test]
fn schema_find_class_threadsafe_missing_slot_maps_to_missing_slot_error() {
    // A scope whose vtable has no ResolveClassInfoThreadsafe entry: the call must map to
    // Error::MissingSlot, not crash. (The mock statics live at file scope so the
    // FindTypeScopeForModule mock below can reference them.)
    // SAFETY: the mock objects are live and their vtables are the generated layout.
    let system = unsafe { SchemaSystem::from_raw((&raw const NO_THREADSAFE_SYSTEM.0).cast_mut()) }
        .expect("mock object non-null");
    let scope = system
        .type_scope("libserver.so")
        .expect("the mock resolves every module name");
    assert!(matches!(
        scope.find_class_threadsafe("CBaseEntity"),
        Err(Error::MissingSlot(
            "ISchemaSystemTypeScope::ResolveClassInfoThreadsafe"
        ))
    ));
}

/// A scope whose vtable has no `ResolveClassInfoThreadsafe` entry (missing-slot test).
static NO_THREADSAFE_VTABLE: Mock<sys::CSchemaSystemTypeScope_vtable> =
    Mock(sys::CSchemaSystemTypeScope_vtable {
        base: sys::ISchemaSystemTypeScope_vtable {
            GetScopeName: Some(mock_get_scope_name),
            ..sys::ISchemaSystemTypeScope_vtable::NULL
        },
    });

static NO_THREADSAFE_SCOPE: Mock<sys::CSchemaSystemTypeScope> = Mock(sys::CSchemaSystemTypeScope {
    vtable: &raw const NO_THREADSAFE_VTABLE.0,
});

static NO_THREADSAFE_SCHEMA: Mock<sys::ISchemaSystem_vtable> = Mock(sys::ISchemaSystem_vtable {
    FindTypeScopeForModule: Some(mock_find_type_scope_no_threadsafe),
    ..sys::ISchemaSystem_vtable::NULL
});

static NO_THREADSAFE_SYSTEM: Mock<sys::ISchemaSystem> = Mock(sys::ISchemaSystem {
    vtable: &raw const NO_THREADSAFE_SCHEMA.0,
});

/// `FindTypeScopeForModule` mock handing back the scope *without* the threadsafe slot.
unsafe extern "C" fn mock_find_type_scope_no_threadsafe(
    _this: *mut sys::ISchemaSystem,
    _psz_module_name: *const c_char,
    _ppsz_binding_name: *mut *const c_char,
) -> *mut sys::CSchemaSystemTypeScope {
    (&raw const NO_THREADSAFE_SCOPE.0).cast_mut()
}

#[test]
fn schema_from_raw_rejects_null() {
    // SAFETY: null is the documented "None" input of from_raw.
    assert!(unsafe { SchemaSystem::from_raw(core::ptr::null_mut()) }.is_none());
}

// ---------------------------------------------------------------------------
// negative auto-trait checks (WP5b finding 3a)
// ---------------------------------------------------------------------------

/// Inlined `static_assertions::assert_not_impl_any!` (its stable, proven mechanism): a
/// blanket impl plus a trait-gated impl of the same parameterized trait make the `_` in
/// `<$ty as AmbiguousIfImpl<_>>::some_item` ambiguous to infer exactly when `$ty`
/// implements one of the listed traits — so this **fails to compile** in that case, and the
/// test guards `GameEventManager`/`Listener` against silently growing `Send`/`Sync`.
macro_rules! assert_not_impl {
    ($ty:ty: $($trait:path),+ $(,)?) => {
        const _: fn() = || {
            trait AmbiguousIfImpl<A> {
                // Required for actually being able to reference the trait.
                fn some_item() {}
            }

            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}

            $({
                struct Invalid;
                impl<T: ?Sized + $trait> AmbiguousIfImpl<Invalid> for T {}
            })+

            let _ = <$ty as AmbiguousIfImpl<_>>::some_item;
        };
    };
}

/// The game-event layer must be `!Send + !Sync`: registration and drop are game-thread-only
/// (WP5b finding 3a). The assertions above are compile-time — if the test builds, they hold.
#[test]
fn game_event_manager_and_listener_are_not_send_or_sync() {
    use s2sdk::events::{GameEventManager, Listener};
    assert_not_impl!(GameEventManager: Send, Sync);
    assert_not_impl!(Listener: Send, Sync);
}

// ---------------------------------------------------------------------------
// cvar_dispatch mocks
// ---------------------------------------------------------------------------

/// The `ConVarData` mock the cvar tests read: an Int32 convar whose slot-0 value bytes are
/// `42i32`.
static MOCK_CVAR_DATA_INT32: Mock<sys::ConVarData> = Mock(sys::ConVarData {
    m_pszName: c"sv_cheats".as_ptr(),
    m_defaultValue: core::ptr::null_mut(),
    m_minValue: core::ptr::null_mut(),
    m_maxValue: core::ptr::null_mut(),
    m_pszHelpString: c"Allow cheats on server".as_ptr(),
    m_eVarType: sys::EConVarType::EConVarType_Int32,
    m_Version: 0,
    m_iTimesChanged: 0,
    m_nFlags: 0,
    m_iCallbackIndex: 0,
    m_iFilterCBIndex: 0,
    m_iCompletionCBIndex: 0,
    m_GameInfoFlags: 0,
    m_UserInfoByteIndex: 0,
    _pad14: [0; 4],
    m_fnGetEnumInfoHandle: core::ptr::null(),
    // Slot-0 value bytes: 42 as a little-endian i32 (the rest of the inline array is pad).
    m_Values: {
        let mut bytes = [0u8; 16];
        bytes[0] = 42;
        bytes
    },
});

/// A convar with an untyped tag: `EConVarType(99)` must map to `CvarValue::Unknown`.
static MOCK_CVAR_DATA_UNKNOWN: Mock<sys::ConVarData> = Mock(sys::ConVarData {
    m_pszName: c"mystery".as_ptr(),
    m_defaultValue: core::ptr::null_mut(),
    m_minValue: core::ptr::null_mut(),
    m_maxValue: core::ptr::null_mut(),
    m_pszHelpString: c"".as_ptr(),
    m_eVarType: sys::EConVarType(99),
    m_Version: 0,
    m_iTimesChanged: 0,
    m_nFlags: 0,
    m_iCallbackIndex: 0,
    m_iFilterCBIndex: 0,
    m_iCompletionCBIndex: 0,
    m_GameInfoFlags: 0,
    m_UserInfoByteIndex: 0,
    _pad14: [0; 4],
    m_fnGetEnumInfoHandle: core::ptr::null(),
    m_Values: [0; 16],
});

/// Which data record `GetConVarData` should hand back for the next call.
static NEXT_CVAR_DATA: Mutex<usize> = Mutex::new(0);

/// Serializes the tests that program [`NEXT_CVAR_DATA`]: the guard must be held for the
/// whole test body (the shared mock's `GetConVarData` itself locks [`NEXT_CVAR_DATA`] when
/// it runs, so holding *that* lock across the body would deadlock). Without this test-wide
/// lock, each test's guard would end at its statement and parallel tests would race.
static CVAR_DATA_TEST_LOCK: Mutex<()> = Mutex::new(());

const DATA_INT32: usize = 0;
const DATA_UNKNOWN: usize = 1;
const DATA_NONE: usize = 2;
const DATA_STRING: usize = 3;

/// The mock `ICvar` object.
static MOCK_ICVAR: Mock<sys::ICvar> = Mock(sys::ICvar {
    vtable: &raw const MOCK_CVAR_VTABLE.0,
});

/// The mock `ICvar` vtable: `FindConVar` returns a valid ref, `GetConVarData` returns the
/// record selected in [`NEXT_CVAR_DATA`].
static MOCK_CVAR_VTABLE: Mock<sys::ICvar_vtable> = Mock(sys::ICvar_vtable {
    FindConVar: Some(mock_find_con_var),
    GetConVarData: Some(mock_get_con_var_data),
    ..sys::ICvar_vtable::NULL
});

/// `FindConVar` mock: every name resolves to `{ access index 7, registered index 0 }`.
unsafe extern "C" fn mock_find_con_var(
    _this: *mut sys::ICvar,
    _name: *const c_char,
    _allow_defensive: bool,
) -> sys::ConVarRef {
    sys::ConVarRef {
        m_ConVarAccessIndex: 7,
        _pad1: [0; 2],
        m_ConVarRegisteredIndex: 0,
    }
}

/// The string convar's data record, built once at first use (`m_Values` must hold the real
/// string pointer, which a `static` initializer cannot spell portably).
static STRING_DATA_CELL: OnceLock<Mock<sys::ConVarData>> = OnceLock::new();

fn string_convar_data() -> *mut sys::ConVarData {
    let mock = STRING_DATA_CELL.get_or_init(|| {
        // A leaked string: the pointer is valid for the whole process, so the value copy
        // below is race-free even though the "game thread" here is the test thread.
        let leaked: &'static CStr = Box::leak(
            std::ffi::CString::new("hello world")
                .expect("no interior NUL")
                .into_boxed_c_str(),
        );
        let mut bytes = [0u8; 16];
        let ptr = leaked.as_ptr() as u64;
        bytes[..8].copy_from_slice(&ptr.to_ne_bytes());
        Mock(sys::ConVarData {
            m_pszName: c"host_name".as_ptr(),
            m_defaultValue: core::ptr::null_mut(),
            m_minValue: core::ptr::null_mut(),
            m_maxValue: core::ptr::null_mut(),
            m_pszHelpString: c"Server host name".as_ptr(),
            m_eVarType: sys::EConVarType::EConVarType_String,
            m_Version: 0,
            m_iTimesChanged: 0,
            m_nFlags: 0,
            m_iCallbackIndex: 0,
            m_iFilterCBIndex: 0,
            m_iCompletionCBIndex: 0,
            m_GameInfoFlags: 0,
            m_UserInfoByteIndex: 0,
            _pad14: [0; 4],
            m_fnGetEnumInfoHandle: core::ptr::null(),
            m_Values: bytes,
        })
    });
    (&raw const mock.0).cast_mut()
}

/// `GetConVarData` mock: hand back the selected static record (or null for the "no data"
/// path).
unsafe extern "C" fn mock_get_con_var_data(
    _this: *mut sys::ICvar,
    _cvar: sys::ConVarRef,
) -> *mut sys::ConVarData {
    let selected = *NEXT_CVAR_DATA.lock().unwrap();
    match selected {
        DATA_INT32 => (&raw const MOCK_CVAR_DATA_INT32.0).cast_mut(),
        DATA_UNKNOWN => (&raw const MOCK_CVAR_DATA_UNKNOWN.0).cast_mut(),
        DATA_STRING => string_convar_data(),
        _ => core::ptr::null_mut(),
    }
}

fn mock_cvar() -> Cvar {
    // SAFETY: the mock object is live and its vtable is the generated layout.
    unsafe { Cvar::from_raw((&raw const MOCK_ICVAR.0).cast_mut()) }.expect("mock object non-null")
}

#[test]
fn cvar_dispatch_reads_the_value() {
    // The mock is shared; the selection must stay ours for the whole body (WP5b finding 5).
    let _lock = CVAR_DATA_TEST_LOCK.lock().unwrap();
    *NEXT_CVAR_DATA.lock().unwrap() = DATA_INT32;
    let cvar = mock_cvar();
    let convar = cvar.find("sv_cheats").expect("mock FindConVar resolves");
    assert_eq!(convar.name().unwrap(), "sv_cheats");
    assert_eq!(convar.help().unwrap(), "Allow cheats on server");
    assert_eq!(convar.var_type(), sys::EConVarType::EConVarType_Int32);
    assert_eq!(convar.flags(), 0);
    assert_eq!(convar.value().unwrap(), CvarValue::I32(42));
}

/// The invalid-access-index `FindConVar` mock (`kInvalidAccessIndex` upstream).
unsafe extern "C" fn mock_find_con_var_invalid(
    _this: *mut sys::ICvar,
    _name: *const c_char,
    _allow_defensive: bool,
) -> sys::ConVarRef {
    sys::ConVarRef {
        m_ConVarAccessIndex: 0xFFFF, // upstream ConVarRef::kInvalidAccessIndex
        _pad1: [0; 2],
        m_ConVarRegisteredIndex: 0,
    }
}

#[test]
fn cvar_invalid_access_index_maps_to_not_found() {
    // A ref with the invalid access index must map to Error::NotFound before GetConVarData
    // is ever called (this mock's vtable has no GetConVarData: a call there would be a
    // MissingSlot abort, proving the early return).
    static INVALID_VTABLE: Mock<sys::ICvar_vtable> = Mock(sys::ICvar_vtable {
        FindConVar: Some(mock_find_con_var_invalid),
        ..sys::ICvar_vtable::NULL
    });
    static INVALID_ICVAR: Mock<sys::ICvar> = Mock(sys::ICvar {
        vtable: &raw const INVALID_VTABLE.0,
    });
    // SAFETY: the mock object is live and its vtable is the generated layout.
    let cvar = unsafe { Cvar::from_raw((&raw const INVALID_ICVAR.0).cast_mut()) }.unwrap();
    let err = cvar.find("sv_cheats").unwrap_err();
    assert!(
        matches!(&err, Error::NotFound(name) if name == "sv_cheats"),
        "{err:?}"
    );
}

/// The library probe added for the WP6b probe's `cvar` check: the candidate order is fixed,
/// the per-library step refuses a module that does not serve the version, and outside the game
/// the lookup reports the missing modules (never a wrong interface).
#[test]
fn cvar_probe_uses_the_documented_library_order() {
    assert_eq!(CVAR_LIBS, ["libtier0.so", "libengine2.so"]);

    // `libc.so.6` is mapped in every process and is not a game module: `from_module` comes
    // back None rather than handing out an interface.
    let libc = Module::find_loaded("libc.so.6").expect("libc.so.6 is mapped in every process");
    assert!(Cvar::from_module(&libc).is_none());

    // These tests never run inside CS2: neither candidate is mapped, so `from_process` (and
    // therefore `from_process_with_lib`) reports both library names as missing.
    let err = Cvar::from_process().unwrap_err();
    assert!(
        matches!(&err, Error::ModuleNotLoaded(libs) if libs == "libtier0.so / libengine2.so"),
        "{err:?}"
    );
}

#[test]
fn cvar_unknown_type_tag_maps_to_unknown() {
    let _lock = CVAR_DATA_TEST_LOCK.lock().unwrap();
    *NEXT_CVAR_DATA.lock().unwrap() = DATA_UNKNOWN;
    let cvar = mock_cvar();
    let convar = cvar.find("mystery").expect("mock FindConVar resolves");
    assert_eq!(
        convar.value().unwrap(),
        CvarValue::Unknown(sys::EConVarType(99))
    );
}

#[test]
fn cvar_null_con_var_data_maps_to_null_return() {
    let _lock = CVAR_DATA_TEST_LOCK.lock().unwrap();
    *NEXT_CVAR_DATA.lock().unwrap() = DATA_NONE;
    let cvar = mock_cvar();
    let err = cvar.find("sv_cheats").unwrap_err();
    assert!(
        matches!(err, Error::NullReturn("ICvar::GetConVarData")),
        "{err:?}"
    );
}

/// A string convar: `value()` must *not* dereference the `CUtlString` (the game thread may
/// free it concurrently) — it reports [`CvarValue::StringUnavailableOffThread`].
#[test]
fn cvar_string_value_is_unavailable_off_thread() {
    let _lock = CVAR_DATA_TEST_LOCK.lock().unwrap();
    *NEXT_CVAR_DATA.lock().unwrap() = DATA_STRING;
    let cvar = mock_cvar();
    let convar = cvar.find("host_name").expect("mock FindConVar resolves");
    assert_eq!(convar.var_type(), sys::EConVarType::EConVarType_String);
    assert_eq!(
        convar.value().unwrap(),
        CvarValue::StringUnavailableOffThread
    );
}

/// On the "game thread" (this test thread, with a leaked, never-freed backing string), the
/// explicit unsafe accessor does copy the value.
#[test]
fn cvar_value_string_on_game_thread_copies_the_value() {
    let _lock = CVAR_DATA_TEST_LOCK.lock().unwrap();
    *NEXT_CVAR_DATA.lock().unwrap() = DATA_STRING;
    let cvar = mock_cvar();
    let convar = cvar.find("host_name").expect("mock FindConVar resolves");
    // SAFETY: the mock data record is process-lifetime and its string is leaked — for the
    // duration of this call nothing frees it, which is the game-thread contract.
    let value = unsafe { convar.value_string_on_game_thread() }.expect("string convar copies");
    assert_eq!(value, "hello world");

    // The explicit accessor rejects non-string convars.
    *NEXT_CVAR_DATA.lock().unwrap() = DATA_INT32;
    let scalar = cvar.find("sv_cheats").expect("mock FindConVar resolves");
    let err = unsafe { scalar.value_string_on_game_thread() }.unwrap_err();
    assert!(
        matches!(
            err,
            Error::CvarNotString(sys::EConVarType::EConVarType_Int32)
        ),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// events_dispatch mocks
// ---------------------------------------------------------------------------

/// The listener pointer `AddListener` stored (`0` after `RemoveListener`).
static STORED_LISTENER: Mutex<usize> = Mutex::new(0);

/// What the fired event looked like: `(name, userid)` per dispatch.
static DISPATCHED: Mutex<Vec<(String, i32)>> = Mutex::new(Vec::new());

/// The mock manager's inner vtable: `AddListener` stores the listener pointer,
/// `RemoveListener` clears it.
static MOCK_MANAGER_VTABLE: Mock<sys::IGameEventManager2_vtable> =
    Mock(sys::IGameEventManager2_vtable {
        AddListener: Some(mock_add_listener),
        RemoveListener: Some(mock_remove_listener),
        ..sys::IGameEventManager2_vtable::NULL
    });

/// The mock manager object.
static MOCK_EVENT_MANAGER: Mock<sys::IGameEventManager2> = Mock(sys::IGameEventManager2 {
    vtable: &raw const MOCK_MANAGER_VTABLE.0,
});

/// What the `AddListener` mock returns: an event index (>= 0) or -1 for an unknown event.
static ADD_LISTENER_RET: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(37);

/// `AddListener` mock: store the listener pointer, return [`ADD_LISTENER_RET`].
unsafe extern "C" fn mock_add_listener(
    _this: *mut sys::IGameEventManager2,
    listener: *mut sys::IGameEventListener2,
    _name: *const c_char,
    _b_server_side: bool,
) -> i32 {
    *STORED_LISTENER.lock().unwrap() = listener as usize;
    ADD_LISTENER_RET.load(std::sync::atomic::Ordering::SeqCst)
}

/// `RemoveListener` mock: clear the stored pointer.
unsafe extern "C" fn mock_remove_listener(
    _this: *mut sys::IGameEventManager2,
    listener: *mut sys::IGameEventListener2,
) {
    let mut stored = STORED_LISTENER.lock().unwrap();
    if *stored == listener as usize {
        *stored = 0;
    }
}

/// The mock event object.
static MOCK_GAME_EVENT: Mock<sys::IGameEvent> = Mock(sys::IGameEvent {
    vtable: &raw const MOCK_EVENT_VTABLE.0,
});

/// The mock `IGameEvent` vtable: `GetName` returns "player_death", `GetInt` checks the key
/// hash and returns 5.
static MOCK_EVENT_VTABLE: Mock<sys::IGameEvent_vtable> = Mock(sys::IGameEvent_vtable {
    GetName: Some(mock_get_name),
    GetInt: Some(mock_get_int),
    ..sys::IGameEvent_vtable::NULL
});

/// `GetName` mock: `player_death`.
unsafe extern "C" fn mock_get_name(_this: *mut sys::IGameEvent) -> *const c_char {
    c"player_death".as_ptr()
}

/// What the `GetInt` mock saw last: `(hash, sym_id)`. Recorded instead of asserted inside
/// the mock — an `assert!` in an `extern "C"` mock cannot unwind, it aborts the whole test
/// process (WP5b finding 8).
static RECORDED_KEY: Mutex<(u32, u32)> = Mutex::new((0, 0));

/// `GetInt` mock: record the key symbol, then return 5.
unsafe extern "C" fn mock_get_int(
    _this: *mut sys::IGameEvent,
    key_symbol: *const sys::CKV3MemberName,
    _default_value: i32,
) -> i32 {
    // SAFETY: the caller passes a live `CKV3MemberName` for the call.
    let key = unsafe { key_symbol.read() };
    if let Ok(mut rec) = RECORDED_KEY.lock() {
        *rec = (key.m_nHashCode.m_nHashCode, key.m_SymId);
    }
    5
}

#[test]
fn events_dispatch_fires_the_callback_and_unregisters_on_drop() {
    // SAFETY: the mock object is live and its vtable is the generated layout.
    let manager =
        unsafe { GameEventManager::from_raw((&raw const MOCK_EVENT_MANAGER.0).cast_mut()) }
            .expect("mock object non-null");
    let listener: Listener = manager
        .add_listener("player_death", false, |event| {
            let name = event.name().unwrap();
            let userid = event
                .get_int(&KeySymbol::new(c"userid").unwrap(), -1)
                .unwrap();
            // In production the callback runs on the game thread; in the mock the dispatch
            // happens on the test thread, so recording here is fine.
            DISPATCHED.lock().unwrap().push((name, userid));
        })
        .expect("the mock AddListener returns event index 37");
    // Any non-negative event index is a registration (upstream: "the event's index or -1").
    assert_eq!(listener.event_index(), 37);

    // The engine-side pointer is the boxed Rust object.
    let stored = *STORED_LISTENER.lock().unwrap();
    assert_ne!(stored, 0, "AddListener stored the listener pointer");

    // Fire the event the way the engine would: through the stored object's vtable.
    let obj = stored as *mut sys::IGameEventListener2;
    // SAFETY: the stored pointer is the live boxed ListenerObject (registered above).
    let vt = unsafe { sys::IGameEventListener2::vtable(obj) };
    let fire = vt
        .FireGameEvent
        .expect("the static Rust vtable fills FireGameEvent");
    // SAFETY: the event mock is live for the duration of the call.
    unsafe { fire(obj, (&raw const MOCK_GAME_EVENT.0).cast_mut()) };

    let dispatched = DISPATCHED.lock().unwrap();
    assert_eq!(
        dispatched.last(),
        Some(&("player_death".to_string(), 5)),
        "the closure saw the event name and the userid"
    );
    // The key symbol the callback passed reached the GetInt mock with the murmur hash of
    // the lowercased key and UTL_INVAL_SYMBOL_LARGE (asserted here, in the test body, not
    // inside the `extern "C"` mock — that would abort on failure).
    assert_eq!(
        *RECORDED_KEY.lock().unwrap(),
        (0x3e7b_804b, u32::MAX),
        "the key symbol was the murmur hash of the lowercased key, with UTL_INVAL_SYMBOL_LARGE"
    );

    // Dropping the Listener unregisters: the mock's stored pointer clears.
    drop(listener);
    assert_eq!(
        *STORED_LISTENER.lock().unwrap(),
        0,
        "drop cleared the stored pointer"
    );
}

#[test]
fn key_symbol_hashes_like_upstream() {
    // "userid" (the pinned 0x3e7b804b; also asserted inside the GetInt mock).
    let key = KeySymbol::new(c"userid").unwrap();
    assert_eq!(key.hash(), 0x3e7b_804b);
    // Case-insensitive: "UserID" hashes the same.
    let upper = KeySymbol::new(c"UserID").unwrap();
    assert_eq!(upper.hash(), 0x3e7b_804b);
    // Empty name: upstream's `const char *` constructor leaves the hash code 0.
    let empty = KeySymbol::new(c"").unwrap();
    assert_eq!(empty.hash(), 0);
    assert_eq!(empty.sym_id(), u32::MAX, "UTL_INVAL_SYMBOL_LARGE");
}

#[test]
fn events_missing_slot_maps_to_missing_slot_error() {
    // A manager whose vtable is all-null: every call must hit Error::MissingSlot, not crash.
    static NULL_VTABLE: Mock<sys::IGameEventManager2_vtable> =
        Mock(sys::IGameEventManager2_vtable::NULL);
    static NULL_MANAGER: Mock<sys::IGameEventManager2> = Mock(sys::IGameEventManager2 {
        vtable: &raw const NULL_VTABLE.0,
    });
    // SAFETY: the mock object is live and its vtable is the generated (all-null) layout.
    let manager = unsafe { GameEventManager::from_raw((&raw const NULL_MANAGER.0).cast_mut()) }
        .expect("mock object non-null");
    assert!(matches!(
        manager.add_listener("player_death", false, |_| {}),
        Err(Error::MissingSlot("IGameEventManager2::AddListener"))
    ));
    assert!(matches!(
        manager.lookup_event_id("player_death"),
        Err(Error::MissingSlot("IGameEventManager2::LookupEventId"))
    ));
    assert!(matches!(
        manager.has_event("player_death"),
        Err(Error::MissingSlot("IGameEventManager2::HasEventDescriptor"))
    ));
}

#[test]
fn events_reject_event_maps_to_not_found() {
    // A manager whose AddListener returns -1 (upstream: unknown event): the Rust side must
    // report NotFound and unregister the object before freeing it, so the engine can never
    // keep a dangling pointer.
    static REJECT_VTABLE: Mock<sys::IGameEventManager2_vtable> =
        Mock(sys::IGameEventManager2_vtable {
            AddListener: Some(mock_add_listener_reject),
            RemoveListener: Some(mock_remove_listener_reject),
            ..sys::IGameEventManager2_vtable::NULL
        });
    static REJECT_MANAGER: Mock<sys::IGameEventManager2> = Mock(sys::IGameEventManager2 {
        vtable: &raw const REJECT_VTABLE.0,
    });
    // SAFETY: the mock object is live and its vtable is the generated layout.
    let manager = unsafe { GameEventManager::from_raw((&raw const REJECT_MANAGER.0).cast_mut()) }
        .expect("mock object non-null");
    let err = match manager.add_listener("nope", false, |_| {}) {
        Err(e) => e,
        Ok(_) => panic!("the reject mock must not accept a listener"),
    };
    assert!(
        matches!(&err, Error::NotFound(name) if name == "nope"),
        "{err:?}"
    );
    assert_eq!(
        REJECT_REMOVED.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the rejected listener was removed once before it was freed"
    );
}

/// How often the reject manager's `RemoveListener` ran.
static REJECT_REMOVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn mock_remove_listener_reject(
    _this: *mut sys::IGameEventManager2,
    _listener: *mut sys::IGameEventListener2,
) {
    REJECT_REMOVED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// `AddListener` mock that always rejects (returns -1, upstream's "unknown event").
unsafe extern "C" fn mock_add_listener_reject(
    _this: *mut sys::IGameEventManager2,
    _listener: *mut sys::IGameEventListener2,
    _name: *const c_char,
    _b_server_side: bool,
) -> i32 {
    -1
}

/// Listener pointer stored by the refcount test's own manager (separate from the shared
/// `STORED_LISTENER`, so this test cannot race the dispatch test).
static RC_STORED: Mutex<usize> = Mutex::new(0);

unsafe extern "C" fn rc_add_listener(
    _this: *mut sys::IGameEventManager2,
    listener: *mut sys::IGameEventListener2,
    _name: *const c_char,
    _b_server_side: bool,
) -> i32 {
    *RC_STORED.lock().unwrap() = listener as usize;
    3
}

unsafe extern "C" fn rc_remove_listener(
    _this: *mut sys::IGameEventManager2,
    _listener: *mut sys::IGameEventListener2,
) {
    *RC_STORED.lock().unwrap() = 0;
}

#[test]
fn events_listener_survives_repeated_fires_and_is_freed_on_drop() {
    // The engine may fire a listener any number of times; each fire must leave the
    // object's ownership unchanged, and dropping the Listener must free it (no leak).
    // A sentinel `Rc` captured by the callback observes both: it lives exactly as long as
    // the listener object.
    static RC_VTABLE: Mock<sys::IGameEventManager2_vtable> = Mock(sys::IGameEventManager2_vtable {
        AddListener: Some(rc_add_listener),
        RemoveListener: Some(rc_remove_listener),
        ..sys::IGameEventManager2_vtable::NULL
    });
    static RC_MANAGER: Mock<sys::IGameEventManager2> = Mock(sys::IGameEventManager2 {
        vtable: &raw const RC_VTABLE.0,
    });
    // SAFETY: the mock object is live and its vtable is the generated layout.
    let manager = unsafe { GameEventManager::from_raw((&raw const RC_MANAGER.0).cast_mut()) }
        .expect("mock object non-null");
    let sentinel = std::rc::Rc::new(());
    let fires = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let (held, counted) = (sentinel.clone(), fires.clone());
    let listener = manager
        .add_listener("round_start", true, move |_| {
            let _keep = &held;
            counted.set(counted.get() + 1);
        })
        .expect("index 3 is a registration");
    assert_eq!(listener.event_index(), 3);

    let obj = *RC_STORED.lock().unwrap() as *mut sys::IGameEventListener2;
    assert!(!obj.is_null());
    // SAFETY: `obj` is the registered listener object; the event mock is live.
    let fire = unsafe { sys::IGameEventListener2::vtable(obj) }
        .FireGameEvent
        .expect("the static Rust vtable fills FireGameEvent");
    for _ in 0..3 {
        // SAFETY: as above; the listener stays registered across the fires.
        unsafe { fire(obj, (&raw const MOCK_GAME_EVENT.0).cast_mut()) };
    }
    assert_eq!(fires.get(), 3, "every fire reached the callback");
    assert_eq!(
        std::rc::Rc::strong_count(&sentinel),
        2,
        "the listener object (and its callback) is still alive after repeated fires"
    );

    drop(listener);
    assert_eq!(
        *RC_STORED.lock().unwrap(),
        0,
        "drop unregistered the listener"
    );
    assert_eq!(
        std::rc::Rc::strong_count(&sentinel),
        1,
        "dropping the Listener freed the object (no leaked reference)"
    );
}
