//! The registry overlay: [`registry::RUST_OVERRIDES`] replaces C rows for this build while
//! [`registry::METHODS`] stays the frozen C table (`tests/registry.rs` pins that one row for
//! row against the C source; it never reads an override).

use std::collections::HashSet;

use cs2_bridge::control;
use cs2_bridge::registry::{
    self, effective_methods, find, gate_with, Gate, ModState, ModuleView, METHODS, NEED_KHOOK,
    RUST_METHODS, RUST_OVERRIDES,
};

/// The implemented `needs == 0` rows that are not `control.*`: answered on the IPC lane by
/// their op's `serves` (`wiring::BridgeState::route`), never queued to the main thread. The
/// C table's "implemented needs==0 => control.*" invariant (`tests/registry.rs`) stays on the
/// C table; this is its documented exception for this build's own rows.
const LANE_SERVED: &[&str] = &["metamod.plugins", "job.start", "job.status", "job.cancel"];

#[test]
fn every_override_names_a_c_row_once_and_is_not_a_rust_row() {
    let mut seen = HashSet::new();
    for o in RUST_OVERRIDES {
        assert!(
            METHODS.iter().any(|m| m.name == o.name),
            "override `{}` names no C row",
            o.name
        );
        assert!(seen.insert(o.name), "two overrides for `{}`", o.name);
        assert!(
            RUST_METHODS.iter().all(|m| m.name != o.name),
            "`{}` is both an override and a RUST_METHODS row",
            o.name
        );
        assert!(!o.hidden || o.implemented, "`{}`: hidden implies implemented", o.name);
    }
}

/// Overrides whose `needs` may differ from the C row's, each with the reason. Empty today:
/// `job.status` / `job.cancel` keep the C rows' `needs == 0`.
const NEEDS_MAY_DIFFER: &[&str] = &[];

#[test]
fn an_override_keeps_the_c_rows_needs_unless_listed() {
    for o in RUST_OVERRIDES {
        let c = METHODS.iter().find(|m| m.name == o.name).expect("names a C row");
        assert!(
            o.needs == c.needs || NEEDS_MAY_DIFFER.contains(&o.name),
            "override `{}` changes needs ({} -> {}) without being in NEEDS_MAY_DIFFER",
            o.name,
            c.needs,
            o.needs
        );
    }
}

#[test]
fn the_effective_table_has_no_duplicate_names_and_counts_add_up() {
    let names: Vec<&str> = effective_methods().map(|m| m.name).collect();
    let unique: HashSet<&str> = names.iter().copied().collect();
    assert_eq!(names.len(), unique.len(), "duplicate effective names");
    assert_eq!(names.len(), METHODS.len() + RUST_METHODS.len());
    let c_impl = METHODS.iter().filter(|m| m.implemented).count();
    assert_eq!(c_impl, 69, "the C table is untouched");
    let flipped = RUST_OVERRIDES
        .iter()
        .filter(|o| o.implemented && !METHODS.iter().any(|m| m.name == o.name && m.implemented))
        .count();
    let unflipped = RUST_OVERRIDES
        .iter()
        .filter(|o| !o.implemented && METHODS.iter().any(|m| m.name == o.name && m.implemented))
        .count();
    let effective_impl = effective_methods().filter(|m| m.implemented).count();
    assert_eq!(
        effective_impl,
        c_impl + flipped - unflipped + RUST_METHODS.iter().filter(|m| m.implemented).count()
    );
}

#[test]
fn needs_zero_implemented_rows_are_control_or_lane_served() {
    for m in effective_methods() {
        if m.implemented && m.needs & !NEED_KHOOK == 0 {
            assert!(
                m.name.starts_with("control.") || LANE_SERVED.contains(&m.name),
                "`{}`: an implemented needs==0 row must be control.* or lane-served",
                m.name
            );
        }
    }
}

#[test]
fn find_and_gate_return_the_override() {
    for o in RUST_OVERRIDES {
        let f = find(o.name).expect("override is findable");
        assert!(std::ptr::eq(f, o), "`{}`: find returns the override row", o.name);
        let expect_blocked = !o.implemented;
        let g = gate_with(o.name, 0xfff, true);
        match g {
            Gate::Blocked { blocked_on } => {
                assert!(expect_blocked, "`{}` blocked but override implements", o.name);
                assert_eq!(Some(blocked_on), o.blocked_on);
            }
            _ => assert!(!expect_blocked, "`{}`: {g:?}", o.name),
        }
        assert!(std::ptr::eq(registry::effective(METHODS.iter().find(|m| m.name == o.name).unwrap()), o));
    }
}

#[test]
fn map_run_is_blocked_on_run_map() {
    assert_eq!(
        gate_with("map.run", 0xfff, true),
        Gate::Blocked { blocked_on: "superseded by hammer-mcp run_map (VToolMessage command channel)" }
    );
}

#[test]
fn the_advertised_set_has_no_duplicates() {
    let views: Vec<ModuleView> = (0..cs2_bridge::generated::pins::PIN_MODULE_COUNT)
        .map(|_| ModuleView::new(ModState::Verified))
        .collect();
    let caps = control::capabilities_object_filtered(&views, 1, &|_| true);
    let start = caps.find("\"methods\":[").unwrap() + "\"methods\":[".len();
    let end = start + caps[start..].find(']').unwrap();
    let names: Vec<&str> = caps[start..end].split(',').collect();
    let unique: HashSet<&&str> = names.iter().collect();
    assert_eq!(names.len(), unique.len(), "duplicate advertised names: {names:?}");
}
