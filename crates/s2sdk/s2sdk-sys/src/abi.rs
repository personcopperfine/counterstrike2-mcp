//! Vtable slot calculation for the two C++ ABIs CS2 ships: the Itanium ABI (Linux,
//! `x86_64-unknown-linux-gnu`) and the MSVC ABI (Windows/Wine bridge).
//!
//! Both targets are generated: `crate::generated` carries a body per ABI behind
//! `cfg(not(windows))` (the Itanium numbers) and `cfg(windows)` (the MSVC ones) for
//! every record, specialization and vtable class the numbers of the two ABIs disagree
//! on - a differing vtable struct, a one-slot destructor, a reversed overload group -
//! and a single ungated body for every item they agree on. [`Abi::native`] is the ABI
//! of the `cfg` the crate was compiled for, so the generated layout test asserts each
//! half's numbers against the rule below for its own target. The MSVC half is
//! compiled with `cargo check --target x86_64-pc-windows-msvc -p s2sdk-sys`; the
//! Windows *runtime* consumer of these layouts is the bridge DLL (`crates/cs2-bridge`,
//! loaded under Wine), which is why this crate's tests are only ever checked, never run,
//! for that target.
//!
//! Input is the generator's declaration-order method lists (see `../README.md`, the
//! `"vtables"` array of the IR): [`VClass::methods`] holds **this class's own virtuals** in
//! declaration order, exactly how the generator records them. Nothing here reads a stored
//! slot number — slots are recomputed from the C++ ABI rules, so generated MSVC tables can
//! be cross-checked against the bridge's pin table (`crates/cs2-bridge/src/generated/pins.rs`,
//! `tests/abi.rs`).
//!
//! # Rules
//!
//! - Slots of the primary base come first, then this class's new virtuals. Only the
//!   `primary_base` chain is walked: secondary bases / multiple inheritance are out of scope
//!   for now (the IR's `secondary_bases` are ignored here — see WP notes).
//! - A method with `overrides: Some("Base::Name#k")` reuses the base's slot instead of taking
//!   a new one (both ABIs). `slot()` resolves inherited names like C++ does: search the
//!   most-derived class first, then walk the primary base chain.
//! - Virtual destructors: Itanium lays out **2 consecutive slots** (complete-object, then
//!   deleting destructor); `slot()` for the `~Name` entry returns the first. MSVC gives a
//!   destructor **1 slot**. Model a destructor that overrides the base destructor with
//!   `overrides` set too — it then reuses the base's destructor slot like any override.
//! - Overloaded virtuals (several new declarations of one name in one class): MSVC packs the
//!   overloads together at the position of the **first** declaration of that name, in
//!   **reverse declaration order**; Itanium keeps declaration order. Address overloads with
//!   an index suffix: `"Name#0"` is the first declared overload, `"Name#1"` the second, etc.;
//!   a plain `"Name"` means `"Name#0"`. (A declaration that *overrides* a base virtual is not
//!   part of a new-slot group — it takes the base slot; mixed new/override overload groups in
//!   one class are not exercised by the generator's output today.)
//!
//! Const-ness: the functions are plain `fn` — the override resolution would need recursion
//! with loops, which `const fn` does not allow on stable at the MSRV this workspace pins.

/// C++ ABI whose vtable layout should be computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Abi {
    /// Itanium C++ ABI (GCC/clang, Linux).
    Itanium,
    /// Microsoft Visual C++ ABI (Windows).
    Msvc,
}

impl Abi {
    /// The ABI CS2 ships for the target this crate is compiled for.
    #[must_use]
    pub const fn native() -> Abi {
        if cfg!(windows) {
            Abi::Msvc
        } else {
            Abi::Itanium
        }
    }

    /// Slots one destructor occupies under this ABI.
    const fn dtor_slots(self) -> usize {
        match self {
            Abi::Itanium => 2, // complete-object + deleting destructor
            Abi::Msvc => 1,
        }
    }
}

/// One virtual method declaration of a single class, in declaration order.
#[derive(Debug)]
pub struct VMethod {
    /// Declared name; the destructor is `"~Name"`.
    pub name: &'static str,
    /// Whether this is the (unique) virtual destructor of the class.
    pub is_dtor: bool,
    /// `"Base::Name"` when this declaration overrides a base virtual and takes the base's
    /// slot (no new slot). The base method may carry an overload suffix: `"Base::Name#k"`.
    /// `None` = the declaration takes a new slot.
    pub overrides: Option<&'static str>,
}

/// A polymorphic class for slot computation: its own virtuals only (inherited ones live on
/// `primary_base`), the shape of the IR's `vtables[].methods` array.
#[derive(Debug)]
pub struct VClass {
    /// Class name, e.g. `"ISchemaSystem"`.
    pub name: &'static str,
    /// Primary (left-most) base of the single-inheritance chain, if any.
    pub primary_base: Option<&'static VClass>,
    /// This class's own virtuals in declaration order (overrides included; `#if 0` blocks and
    /// commented-out declarations excluded, same as the IR's `decl_index` rule).
    pub methods: &'static [VMethod],
}

/// A lookup key after overload-suffix parsing: base name plus overload index.
#[derive(Debug)]
struct Key<'a> {
    name: &'a str,
    overload: usize,
}

/// Split `"Name#2"` into `("Name", 2)`; `"Name"` becomes `("Name", 0)`.
fn parse_key(method: &str) -> Key<'_> {
    if let Some(pos) = method.rfind('#') {
        let idx = &method[pos + 1..];
        if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) {
            return Key {
                name: &method[..pos],
                overload: idx.parse().unwrap_or(usize::MAX),
            };
        }
    }
    Key {
        name: method,
        overload: 0,
    }
}

/// Split `"Base::Name#2"` into `(Some("Base"), Key{ "Name", 2 })`; without `"::"` the whole
/// string is treated as the method name in the immediately enclosing base.
fn parse_override(overrides: &str) -> (Option<&str>, Key<'_>) {
    match overrides.find("::") {
        Some(pos) => (Some(&overrides[..pos]), parse_key(&overrides[pos + 2..])),
        None => (None, parse_key(overrides)),
    }
}

/// Does this declaration take a new slot in its own class? Overrides (including overriding
/// destructors) reuse the base slot.
fn takes_new_slot(m: &VMethod) -> bool {
    m.overrides.is_none()
}

/// Number of vtable entries one declaration consumes.
fn method_size(abi: Abi, m: &VMethod) -> usize {
    if m.is_dtor {
        abi.dtor_slots()
    } else {
        1
    }
}

/// Number of new slots this class adds over its primary base.
fn own_new_slots(abi: Abi, class: &VClass) -> usize {
    class
        .methods
        .iter()
        .filter(|m| takes_new_slot(m))
        .map(|m| method_size(abi, m))
        .sum()
}

/// Length of the class's own vtable (entries from the address point), including every
/// primary base's slots down the chain.
#[must_use]
pub fn vtable_len(abi: Abi, class: &VClass) -> usize {
    let base = match class.primary_base {
        Some(b) => vtable_len(abi, b),
        None => 0,
    };
    base + own_new_slots(abi, class)
}

/// Slot (from the address point: slot 0 = the first virtual entry) of `method` — e.g.
/// `"Init"`, `"~ISchemaSystem"` or the overload address `"Foo#1"` — in `class`'s vtable.
/// Searches the most-derived class first, then the primary base chain.
#[must_use]
pub fn slot(abi: Abi, class: &VClass, method: &str) -> Option<usize> {
    slot_walk(abi, class, parse_key(method), 0)
}

/// Chain walk with a depth guard (an override target can restart the walk one class higher,
/// so finite static data could still cycle; give up instead of hanging).
fn slot_walk(abi: Abi, class: &VClass, key: Key<'_>, depth: usize) -> Option<usize> {
    if depth > 64 {
        return None;
    }
    let mut cur = Some(class);
    while let Some(c) = cur {
        if let Some(s) = slot_in_class(abi, c, key.name, key.overload, depth) {
            return Some(s);
        }
        cur = c.primary_base;
    }
    None
}

/// Slot of the `overload`-th declaration of `name` in one class only (`None` if this class
/// does not declare it; the caller continues down the base chain).
fn slot_in_class(
    abi: Abi,
    class: &VClass,
    name: &str,
    overload: usize,
    depth: usize,
) -> Option<usize> {
    // Which of this class's own declarations does (name, overload index) point at?
    let mut seen = 0usize;
    let target = class.methods.iter().find(|m| {
        if m.name == name {
            let n = seen;
            seen += 1;
            n == overload
        } else {
            false
        }
    })?;

    if let Some(overrides) = target.overrides {
        // Reuses a base slot: follow "Base::Name[#k]" upward instead of laying out a slot.
        let (base_name, key) = parse_override(overrides);
        let mut cur = Some(class);
        while let Some(c) = cur {
            if Some(c.name) == base_name {
                return slot_walk(abi, c, key, depth + 1);
            }
            cur = c.primary_base;
        }
        // Unresolvable override target: treat the declaration as new (documented fallback).
    }
    Some(own_slot(abi, class, target))
}

/// Slot assigned to a new-slot declaration inside `class`, counted from the end of the
/// primary base's vtable.
fn own_slot(abi: Abi, class: &VClass, target: &VMethod) -> usize {
    let base_len = match class.primary_base {
        Some(b) => vtable_len(abi, b),
        None => 0,
    };

    let mut before = 0usize;
    if abi == Abi::Msvc {
        // MSVC: all overloads of one name sit together at the position of the FIRST
        // declaration of that name, in reverse declaration order. A method whose name group
        // starts earlier is fully laid out before the target's group, so adding up every
        // new-slot declaration of every *earlier* group gives the group's start offset;
        // within the target's group the reversed position is the count of same-name
        // declarations *after* the target.
        let target_first = first_decl_index(class, target);
        let mut after_same_name = 0usize;
        let mut seen_target = false;
        for m in class.methods {
            if !takes_new_slot(m) {
                continue;
            }
            if first_decl_index(class, m) < target_first {
                before += method_size(abi, m);
            } else if m.name == target.name && seen_target {
                after_same_name += 1;
            }
            if core::ptr::eq(m, target) {
                seen_target = true;
            }
        }
        return base_len + before + after_same_name;
    }

    // Itanium: plain declaration order, dtors spanning 2 slots.
    for m in class.methods {
        if core::ptr::eq(m, target) {
            break;
        }
        if takes_new_slot(m) {
            before += method_size(abi, m);
        }
    }
    base_len + before
}

/// Declaration index of the first new-slot declaration of `target`'s name (the MSVC group
/// anchor).
fn first_decl_index(class: &VClass, target: &VMethod) -> Option<usize> {
    class
        .methods
        .iter()
        .position(|m| takes_new_slot(m) && m.name == target.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn m(name: &'static str) -> VMethod {
        VMethod {
            name,
            is_dtor: false,
            overrides: None,
        }
    }

    const fn dtor(name: &'static str) -> VMethod {
        VMethod {
            name,
            is_dtor: true,
            overrides: None,
        }
    }

    const fn ovr(name: &'static str, target: &'static str) -> VMethod {
        VMethod {
            name,
            is_dtor: false,
            overrides: Some(target),
        }
    }

    // Fixture: 3 overloads of one name interleaved with other methods, plus a dtor.
    //
    //   declaration order: A, Ov, B, Ov, C, Ov, ~X
    //
    // Itanium (declaration order, dtor = 2 slots):
    //   A=0, Ov#0=1, B=2, Ov#1=3, C=4, Ov#2=5, ~X=6..7
    // MSVC (overload group "Ov" anchored at the first Ov decl = slot 1, reverse order
    // within the group; dtor = 1 slot):
    //   A=0, Ov#2=1, Ov#1=2, Ov#0=3, B=4, C=5, ~X=6
    static OVERLOADS: VClass = VClass {
        name: "X",
        primary_base: None,
        methods: &[
            m("A"),
            m("Ov"),
            m("B"),
            m("Ov"),
            m("C"),
            m("Ov"),
            dtor("~X"),
        ],
    };

    // 3-level chain with a base dtor and an override:
    //
    //   Base: ~Base=0 (2 slots itanium / 1 msvc), Connect=dtor_slots, Extra=dtor_slots+1
    //   Mid:  Connect overrides Base::Connect (no new slot), New1=dtor_slots+2
    //   Leaf: New2=dtor_slots+3
    static BASE: VClass = VClass {
        name: "Base",
        primary_base: None,
        methods: &[dtor("~Base"), m("Connect"), m("Extra")],
    };
    static MID: VClass = VClass {
        name: "Mid",
        primary_base: Some(&BASE),
        methods: &[ovr("Connect", "Base::Connect"), m("New1")],
    };
    static LEAF: VClass = VClass {
        name: "Leaf",
        primary_base: Some(&MID),
        methods: &[m("New2")],
    };

    // A derived destructor overrides the base destructor: it reuses the base slot and
    // takes no new slots (both ABIs behave identically here).
    static BASE2: VClass = VClass {
        name: "Base",
        primary_base: None,
        methods: &[dtor("~Base"), m("Connect")],
    };
    static DERIVED: VClass = VClass {
        name: "Derived",
        primary_base: Some(&BASE2),
        methods: &[VMethod {
            name: "~Derived",
            is_dtor: true,
            overrides: Some("Base::~Base"),
        }],
    };

    // Missing names and unknown overload indexes return None.
    static SMALL: VClass = VClass {
        name: "X",
        primary_base: None,
        methods: &[m("Connect"), m("Ov")],
    };

    #[test]
    fn overload_groups_both_abis() {
        // Itanium: declaration order; plain name = #0.
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "A"), Some(0));
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "Ov"), Some(1));
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "Ov#0"), Some(1));
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "B"), Some(2));
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "Ov#1"), Some(3));
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "C"), Some(4));
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "Ov#2"), Some(5));
        // Dtor: first of the two consecutive slots (the declared name is "~X").
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "~X"), Some(6));
        assert_eq!(slot(Abi::Itanium, &OVERLOADS, "~Base"), None);
        assert_eq!(vtable_len(Abi::Itanium, &OVERLOADS), 8);

        // MSVC: the "Ov" overloads form one group of 3 slots anchored at the first Ov
        // declaration (slot 1), laid out in reverse decl order; B/C/dtor follow after
        // the group.
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "A"), Some(0));
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "Ov#2"), Some(1));
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "Ov#1"), Some(2));
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "Ov"), Some(3));
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "Ov#0"), Some(3));
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "B"), Some(4));
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "C"), Some(5));
        assert_eq!(slot(Abi::Msvc, &OVERLOADS, "~X"), Some(6));
        assert_eq!(vtable_len(Abi::Msvc, &OVERLOADS), 7);
    }

    #[test]
    fn chain_dtor_in_base_and_override() {
        for abi in [Abi::Itanium, Abi::Msvc] {
            let dtor_len = abi.dtor_slots();
            // Base: dtor first, then Connect, then Extra (Mid's Connect overrides the
            // base slot, New1 lands after Extra, Leaf's New2 at the very end).
            assert_eq!(slot(abi, &LEAF, "~Base"), Some(0));
            assert_eq!(slot(abi, &LEAF, "Connect"), Some(dtor_len));
            assert_eq!(slot(abi, &LEAF, "Extra"), Some(dtor_len + 1));
            assert_eq!(slot(abi, &LEAF, "New1"), Some(dtor_len + 2));
            assert_eq!(slot(abi, &LEAF, "New2"), Some(dtor_len + 3));
            assert_eq!(vtable_len(abi, &LEAF), dtor_len + 4);
        }

        // The itanium dtor also exposes the second (deleting) slot in the vtable length.
        assert_eq!(vtable_len(Abi::Itanium, &BASE), 4); // 2 dtor + Connect + Extra
        assert_eq!(vtable_len(Abi::Msvc, &BASE), 3); // 1 dtor + Connect + Extra
    }

    #[test]
    fn derived_dtor_overrides_base_dtor() {
        for abi in [Abi::Itanium, Abi::Msvc] {
            assert_eq!(slot(abi, &DERIVED, "~Derived"), Some(0));
            assert_eq!(slot(abi, &DERIVED, "Connect"), Some(abi.dtor_slots()));
            assert_eq!(vtable_len(abi, &DERIVED), abi.dtor_slots() + 1);
        }
    }

    #[test]
    fn unknown_methods() {
        for abi in [Abi::Itanium, Abi::Msvc] {
            assert_eq!(slot(abi, &SMALL, "Nope"), None);
            assert_eq!(slot(abi, &SMALL, "Ov#1"), None);
            assert_eq!(slot(abi, &SMALL, "Ov#99"), None);
        }
        // A malformed suffix is treated as part of the name (no panic).
        assert_eq!(slot(Abi::Msvc, &SMALL, "Ov#x"), None);
    }

    // The native ABI selector is consistent with the cfg it is derived from.
    #[test]
    fn native_matches_cfg() {
        assert_eq!(
            Abi::native(),
            if cfg!(windows) {
                Abi::Msvc
            } else {
                Abi::Itanium
            }
        );
    }
}
