//! Cross-checks the vtable slot calculator against `bridge/bridge_pins.h` (this lives in
//! the cs2-mcp checkout, three levels above `CARGO_MANIFEST_DIR`) and against the generated
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

/// The bridge pin row must pin the method at slot 13, matching the derived slot.
#[test]
fn bridge_pins_find_type_scope_for_module() {
    let path = format!("{}/../../../bridge/bridge_pins.h", env!("CARGO_MANIFEST_DIR"));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("skipping bridge_pins.h cross-check: cannot read {path}: {err}");
            return;
        }
    };

    // Row shape: { 8, SSV_SCHEMASYSTEM_VT, 13, 0x11350, "ISchemaSystem::FindTypeScopeForModule" },
    let rows: Vec<(usize, &str)> = text
        .lines()
        .filter(|line| line.contains("SSV_SCHEMASYSTEM_VT") && line.contains("ISchemaSystem::"))
        .map(|line| {
            let inner = line.split('{').nth(1).and_then(|r| r.split('}').next());
            let fields: Vec<&str> = inner.unwrap_or("").split(',').collect();
            let pin: usize = fields[2].trim().parse().expect("bridge pin slot field");
            let quoted = fields[4].trim();
            let name = quoted
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .expect("bridge pin quoted name");
            (pin, name)
        })
        .collect();

    let row = rows
        .iter()
        .find(|(_, name)| *name == "ISchemaSystem::FindTypeScopeForModule")
        .expect("no ISchemaSystem::FindTypeScopeForModule row in bridge_pins.h");

    // The row pins slot 13, and the ABI derivation computes exactly that slot.
    assert_eq!(row.0, 13, "bridge_pins.h pins FindTypeScopeForModule at 13");
    assert_eq!(
        slot(Abi::Msvc, &ISCHEMASYSTEM, "FindTypeScopeForModule"),
        Some(row.0)
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
