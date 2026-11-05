//! Cross-checks the vtable slot calculator against the bridge's generated pin table
//! (`crates/cs2-bridge/src/generated/pins.rs`, included below by path, so a missing or
//! reshaped table fails the build instead of skipping the check) and against the generated
//! `VClass` statics (`s2sdk_sys::generated`, built from the upstream headers by
//! `gen/gen.py` — names only, in declaration order, with `overrides` copied verbatim):
//!
//! - `public/appframework/iappsystem.h` — `IAppSystem`: 11 own virtuals, no dtor.
//! - `public/schemasystem/ischemasystem.h` — `ISchemaSystem : public IAppSystem`: 28 own
//!   virtuals in declaration order, then the pure-virtual destructor last.
//!
//! Derivation: `IAppSystem` contributes slots 0..=10 (no dtor under either ABI), so
//! `ISchemaSystem`'s new virtuals start at slot 11 — `GlobalTypeScope`=11,
//! `FindOrCreateTypeScopeForModule`=12, `FindTypeScopeForModule`=13, `GetTypeScopeForBinding`=14,
//! … and the destructor is the last own declaration (MSVC: slot 39, one slot; Itanium: slots
//! 39..40). The bridge pins `ISchemaSystem::FindTypeScopeForModule` at slot 13 on the MSVC
//! vtable — exactly what the rules produce.
//!
//! The second cross-check is the destructor: `IBaseInterface` (one virtual destructor)
//! contributes **two** entries under Itanium and **one** under MSVC, so every method
//! `IGameEventManager2` adds after the inherited destructor sits one slot lower on MSVC.

use s2sdk_sys::abi::{slot, vtable_len, Abi};
use s2sdk_sys::generated::{
    IAppSystem_VCLASS as IAPPSYSTEM, IGameEventManager2_VCLASS,
    ISchemaSystem_VCLASS as ISCHEMASYSTEM,
};

/// The slot the ABI rules produce for the pinned method, on both ABIs (IAppSystem has no
/// destructor, so both start `ISchemaSystem`'s own virtuals at 11).
#[test]
fn find_type_scope_for_module_derives_to_slot_13() {
    assert_eq!(slot(Abi::Msvc, &ISCHEMASYSTEM, "GlobalTypeScope"), Some(11));
    assert_eq!(
        slot(Abi::Msvc, &ISCHEMASYSTEM, "FindOrCreateTypeScopeForModule"),
        Some(12)
    );
    assert_eq!(
        slot(Abi::Msvc, &ISCHEMASYSTEM, "FindTypeScopeForModule"),
        Some(13)
    );
    // Itanium agrees here: IAppSystem has no dtor, so the own-decl offset is identical.
    assert_eq!(
        slot(Abi::Itanium, &ISCHEMASYSTEM, "FindTypeScopeForModule"),
        Some(13)
    );
    // Sanity: 11 base + 28 own virtuals + 1 dtor (MSVC) / 2 dtor slots (Itanium).
    assert_eq!(vtable_len(Abi::Msvc, &ISCHEMASYSTEM), 40);
    assert_eq!(vtable_len(Abi::Itanium, &ISCHEMASYSTEM), 41);
    // The base chain is the generated one, not a transcription.
    assert_eq!(IAPPSYSTEM.name, "IAppSystem");
    assert!(ISCHEMASYSTEM.primary_base.is_some());
}

/// The bridge's pin table, included rather than copied: the rows the pins generator writes
/// from the sigs database (the binary measurement), independent of the headers `gen.py` reads.
#[path = "../../../cs2-bridge/src/generated/pins.rs"]
#[allow(dead_code)]
mod pins;

/// The bridge's `PIN_SLOTS` row for `ISchemaSystem::FindTypeScopeForModule` must pin slot 13 on
/// the schemasystem.dll vtable, the slot the ABI derivation computes from the headers.
#[test]
fn bridge_pins_find_type_scope_for_module() {
    let row = pins::PIN_SLOTS
        .iter()
        .find(|row| row.what == "ISchemaSystem::FindTypeScopeForModule")
        .expect("no ISchemaSystem::FindTypeScopeForModule row in pins.rs PIN_SLOTS");

    // The row reads the schema system's own vtable (the identity row's RVA)...
    assert_eq!(row.module, pins::PIN_SCHEMA, "the row's module is schemasystem.dll");
    assert_eq!(
        u64::from(row.vt_rva),
        pins::SSV_SCHEMASYSTEM_VT.value,
        "the row's vtable is SSV_SCHEMASYSTEM_VT"
    );
    assert!(
        pins::PIN_IDENTS
            .iter()
            .any(|ident| ident.what == "SSV_SCHEMASYSTEM_VT"
                && ident.module == pins::PIN_SCHEMA
                && ident.rva == row.vt_rva),
        "SSV_SCHEMASYSTEM_VT has its PIN_IDENTS identity row"
    );
    // ...and pins slot 13, exactly the slot the ABI derivation computes.
    assert_eq!(row.slot, 13, "pins.rs pins FindTypeScopeForModule at 13");
    assert_eq!(
        slot(Abi::Msvc, &ISCHEMASYSTEM, "FindTypeScopeForModule"),
        Some(usize::from(row.slot))
    );
}

/// One-slot (MSVC) versus two-slot (Itanium) destructors, measured through the inherited
/// `IBaseInterface` destructor of `IGameEventManager2`.
#[test]
fn inherited_destructor_widens_the_itanium_vtable() {
    // The override target: IGameEventManager2's own destructor reuses the base slot.
    let dtor = &IGameEventManager2_VCLASS.methods[0];
    assert!(dtor.is_dtor);
    assert_eq!(dtor.overrides, Some("IBaseInterface::~IBaseInterface"));

    // MSVC: 1 dtor slot, then LoadEventsFromFile, Reset, AddListener.
    assert_eq!(
        slot(Abi::Msvc, &IGameEventManager2_VCLASS, "AddListener"),
        Some(3)
    );
    // Itanium: 2 dtor slots push everything after it down by one.
    assert_eq!(
        slot(Abi::Itanium, &IGameEventManager2_VCLASS, "AddListener"),
        Some(4)
    );
    assert_eq!(vtable_len(Abi::Msvc, &IGameEventManager2_VCLASS), 16);
    assert_eq!(vtable_len(Abi::Itanium, &IGameEventManager2_VCLASS), 17);
}
