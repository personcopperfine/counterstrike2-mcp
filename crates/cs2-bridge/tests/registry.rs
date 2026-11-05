//! The method registry's self-test and its differential against the C table.
//!
//! 1. [`handoff_numbers_and_table_invariants`]: 84 rows, 69 implemented, 1 hidden (the
//!    HANDOFF numbers - the differential below pins them against the C anyway), every name
//!    unique, every `needs` a subset of the defined bits, and every implemented `needs == 0`
//!    row a `control.*` method (the ones the fast path dispatches).
//! 2. [`table_matches_the_c_row_for_row`]: the generator (the C's `k_methods[]`, compiled out
//!    of the frozen source by `common`) prints one METHOD record per row; every field of
//!    every row must equal this port's.
//! 3. [`the_generator_output_compare_is_not_vacuous`]: one flipped hex digit in the C output
//!    must break the comparison - the comparator compares.

mod common;

use cs2_bridge::generated::pins as gen_pins;
use cs2_bridge::registry::{
    find, gate, gate_with, verified_mask, Gate, Method, ModState, ModuleView, METHODS,
    METHOD_COUNT, NEED_CLIENT, NEED_ENGINE, NEED_HAMMER, NEED_INPUT, NEED_KHOOK, NEED_MODELDOC,
    NEED_PET, NEED_RENDER, NEED_RSV, NEED_SCENE, NEED_SCHEMA, NEED_SERVER, NEED_SOUND, NEED_TF2,
    NEED_TIER0,
};
use std::sync::OnceLock;

/// Every defined base bit (`mcp_bridge.c:1030-1044`); a row's `needs` must be a subset.
const ALL_BITS: u32 = NEED_TF2
    | NEED_HAMMER
    | NEED_SCENE
    | NEED_ENGINE
    | NEED_RSV
    | NEED_CLIENT
    | NEED_SERVER
    | NEED_TIER0
    | NEED_SCHEMA
    | NEED_SOUND
    | NEED_MODELDOC
    | NEED_PET;

/// Every bit this build defines: the C's module bits plus this build's own `NEED_KHOOK`.
const ALL_BITS_WITH_RUST: u32 = ALL_BITS | NEED_KHOOK;

#[test]
fn handoff_numbers_and_table_invariants() {
    assert_eq!(METHODS.len(), 84, "the C table has 84 rows (the differential pins each one)");
    assert_eq!(METHODS.len(), METHOD_COUNT, "METHOD_COUNT must match the table");
    assert_eq!(
        METHODS.iter().filter(|m| m.implemented).count(),
        69,
        "the HANDOFF number: 69 implemented rows"
    );
    assert_eq!(
        METHODS.iter().filter(|m| m.hidden).count(),
        1,
        "the HANDOFF number: exactly 1 hidden row"
    );
    let mut names: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
    names.sort_unstable();
    let unique = names.len();
    names.dedup();
    assert_eq!(names.len(), unique, "every method name must be unique");

    for m in METHODS {
        assert_eq!(
            m.needs & !ALL_BITS_WITH_RUST,
            0,
            "row {}: needs must be a subset of the defined bits (mcp_bridge.c:1030-1044 plus \
             this build's NEED_KHOOK)",
            m.name
        );
        assert!(
            !m.hidden || m.implemented,
            "row {}: a hidden row must still be implemented (hidden rows are dropped from advertisements, not from dispatch)",
            m.name
        );
        if m.needs & !NEED_KHOOK == 0 && m.implemented {
            assert!(
                m.name.starts_with("control."),
                "row {}: an implemented needs==0 row must be a control.* method (the fast path dispatches them; mcp_bridge.c:6497-6623)",
                m.name
            );
        }
    }
}

/// The generator's records, cached across the tests in this binary.
fn generator_lines() -> &'static Vec<String> {
    static LINES: OnceLock<Vec<String>> = OnceLock::new();
    LINES.get_or_init(common::run_generator)
}

#[test]
fn table_matches_the_c_row_for_row() {
    let lines = generator_lines();
    let method_lines: Vec<&String> =
        lines.iter().filter(|l| l.starts_with("METHOD\t") && !l.starts_with("METHODCOUNT")).collect();
    let count_line = lines
        .iter()
        .find(|l| l.starts_with("METHODCOUNT\t"))
        .expect("the generator must print METHODCOUNT");
    let c_count: usize =
        count_line.split('\t').nth(1).expect("METHODCOUNT carries a number").parse().expect("a number");
    assert_eq!(c_count, METHODS.len(), "the C table and the Rust table must have the same length");
    assert_eq!(method_lines.len(), METHODS.len(), "one METHOD record per row");

    for (i, line) in method_lines.iter().enumerate() {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 6, "one METHOD record has six fields: {line}");
        let m: &Method = &METHODS[i];
        assert_eq!(m.name, f[1], "row {i}: name");
        // NEED_KHOOK is Rust-only (the C table has no such bit): every row's C bits must match
        // the C exactly, and the Rust bit may only be added on top.
        let c_needs: u32 = f[2].parse().expect("a numeric needs");
        assert_eq!(
            m.needs & !NEED_KHOOK,
            c_needs,
            "row {i}: needs (the C's bits; NEED_KHOOK is this build's own)"
        );
        assert_eq!(m.implemented, f[3] == "1", "row {i}: impl (mcp_bridge.c:1048)");
        assert_eq!(m.hidden, f[4] == "1", "row {i}: hidden (mcp_bridge.c:1049)");
        assert_eq!(
            m.blocked_on.unwrap_or("-"),
            f[5],
            "row {i}: blockedOn (mcp_bridge.c:1050)"
        );
    }
}

#[test]
fn the_generator_output_compare_is_not_vacuous() {
    // The C generator runs with the not_loaded scenario in effect when the table is printed;
    // corrupt one hex digit of one METHOD record and the comparison must break.
    let lines = generator_lines();
    let first = lines.iter().find(|l| l.starts_with("METHOD\t")).expect("a METHOD record");
    let mut fields: Vec<String> = first.split('\t').map(str::to_owned).collect();
    let last = fields.last_mut().expect("a blockedOn field");
    if last.as_str() == "-" {
        *last = "corrupted".to_string();
    } else {
        last.push('x');
    }
    let corrupted = fields.join("\t");
    let m = &METHODS[0];
    let rust_row = format!(
        "METHOD\t{}\t{}\t{}\t{}\t{}",
        m.name,
        m.needs,
        u8::from(m.implemented),
        u8::from(m.hidden),
        m.blocked_on.unwrap_or("-")
    );
    assert_ne!(corrupted, rust_row, "a corrupted C record must not compare equal");
}

#[test]
fn find_and_gate_behave_like_the_c() {
    // find (mcp_bridge.c:1144-1147)
    assert!(find("doc.active").is_some());
    assert!(find("no.such.method").is_none());
    assert!(find("control.bogus").is_none());

    // the gate with nothing verified (mcp_bridge.c:5669-5698)
    assert_eq!(gate("control.bogus", 0), Gate::Unknown);
    assert_eq!(gate("no.such.method", 0), Gate::Unknown);
    assert_eq!(gate("control.ping", 0), Gate::Run, "needs == 0: the fast path (mcp_bridge.c:6618-6621)");
    assert_eq!(
        gate("doc.active", 0),
        Gate::Prerequisite { module: "hammer.dll", pin: gen_pins::PIN_HAMMER },
        "the named module is the first unmet need in the C's bits[] order (mcp_bridge.c:5680-5685)"
    );
    assert_eq!(gate("doc.list", 0), Gate::Blocked { blocked_on: "bridge-protocol.md §7 doc.list (not implemented in M2.2)" });
    assert_eq!(gate("doc.active", NEED_HAMMER), Gate::Run);
    assert_eq!(gate("doc.active", NEED_HAMMER | NEED_PET), Gate::Run, "extra verified bits do not matter");

    // NEED_* are the C's #defines (mcp_bridge.c:1030-1044)
    assert_eq!(NEED_RENDER, NEED_SCENE | NEED_ENGINE | NEED_RSV);
    assert_eq!(NEED_INPUT, NEED_CLIENT | NEED_ENGINE);

    // world.render needs scene+engine+rsv: the first unmet need in bits[] order is scenesystem
    assert_eq!(
        gate("world.render", NEED_HAMMER),
        Gate::Prerequisite { module: "scenesystem.dll", pin: gen_pins::PIN_SCENESYSTEM }
    );
    // game.local needs client+engine+schema: first unmet in bits[] order is engine2
    assert_eq!(
        gate("game.local", NEED_CLIENT | NEED_SCHEMA),
        Gate::Prerequisite { module: "engine2.dll", pin: gen_pins::PIN_ENGINE2 }
    );
    // pet.load is the first row that needs toolframework2 alone (mcp_bridge.c:5679)
    assert_eq!(
        gate("pet.load", 0),
        Gate::Prerequisite { module: "toolframework2.dll", pin: gen_pins::PIN_TF2 }
    );

    // NEED_KHOOK (Rust-only): with every module verified but no IKHook installed, the one row
    // that installs a KHook hook (`game.saveRestore`) answers -32017 with the blockedOn text;
    // with KHook the same mask lets it through.
    let all = ALL_BITS;
    assert_eq!(
        gate_with("game.saveRestore", all, false),
        Gate::KhookMissing,
        "the row walks the KHook path"
    );
    assert_eq!(
        gate_with("game.saveRestore", all, true),
        Gate::Run,
        "KHook present"
    );
    // A missing *module* is still named first: the row cannot run either way, and the module is
    // the more specific answer.
    assert_eq!(
        gate_with("game.saveRestore", 0, false),
        Gate::Prerequisite { module: "server.dll", pin: gen_pins::PIN_SERVER }
    );
    // Every off-ramp answers Run without KHook: refusing one would leave a forced button (or an
    // installed sound wrap) with no way to clear it after a Metamod unload (`registry.rs`'s
    // NEED_KHOOK docs). The three rows that INSTALL a KHook hook are the other half of the rule.
    for method in [
        "input.clear",
        "input.status",
        "input.uninstall",
        "sound.status",
        "sound.drain",
        "sound.uninstall",
    ] {
        assert_eq!(
            gate_with(method, all, false),
            Gate::Run,
            "{method}: an off-ramp row never needs KHook"
        );
    }
    for method in ["game.saveRestore", "input.set", "sound.subscribe"] {
        assert_eq!(
            gate_with(method, all, false),
            Gate::KhookMissing,
            "{method}: the row installs a KHook hook"
        );
        assert_eq!(gate_with(method, all, true), Gate::Run, "{method}: KHook present");
    }
    // And the process-wide `gate` asks the real API slot: no mock is installed in this binary.
    assert_eq!(gate("game.saveRestore", all), Gate::KhookMissing);
}

#[test]
fn verified_mask_follows_the_module_states() {
    let not_loaded: [ModuleView; 12] =
        core::array::from_fn(|_| ModuleView::new(ModState::NotLoaded));
    assert_eq!(verified_mask(&not_loaded), 0);
    let all_verified: [ModuleView; 12] =
        core::array::from_fn(|_| ModuleView::new(ModState::Verified));
    assert_eq!(verified_mask(&all_verified), ALL_BITS);
    // a mismatched or verifying module contributes nothing (mcp_bridge.c:1151, mod_ok)
    let mut mixed: [ModuleView; 12] =
        core::array::from_fn(|_| ModuleView::new(ModState::Verified));
    mixed[usize::from(gen_pins::PIN_HAMMER)] = ModuleView::new(ModState::Mismatch);
    mixed[usize::from(gen_pins::PIN_TF2)] = ModuleView::new(ModState::Verifying);
    assert_eq!(verified_mask(&mixed), ALL_BITS & !NEED_HAMMER & !NEED_TF2);
    // the wrong length fails loudly - never a guess
    let result = std::panic::catch_unwind(|| verified_mask(&not_loaded[..11]));
    assert!(result.is_err(), "a short module slice must panic, not shrink the mask");
}
