//! The control reply builders' Rust-only rules and the gate's negative.
//!
//! The byte-for-byte differential of `control.capabilities` / `hello` / `ping` / `status` /
//! `version` and the gate arms against the C builders went with the C bridge (git history,
//! commit 8cdea42); what stays is what the Rust build adds on top of the C shapes (the
//! `"relaxed"` member) and the NEGATIVE the gate must keep: it never answers `Run` for a method
//! whose needs module is not verified, and it names the module the C's `bits[]` order names first.

use cs2_bridge::control;
use cs2_bridge::generated::pins as gen_pins;
use cs2_bridge::registry::{gate_with, Gate, ModState, ModuleView};

/// The Rust-only `"relaxed"` member of a module object: present only when the KHook-aware
/// identity check relaxed a pin for that module, absent (not `[]`) otherwise - which is what
/// keeps the bytes identical to the C's module object for every module with nothing relaxed.
#[test]
fn module_relaxed_list_appears_only_when_non_empty() {
    let mut views: [ModuleView; gen_pins::PIN_MODULE_COUNT] =
        core::array::from_fn(|_| ModuleView::new(ModState::Verified));
    let caps = control::capabilities_object_filtered(&views, 1, &|_| true);
    assert!(
        !caps.contains("\"relaxed\""),
        "an empty list is omitted: {caps}"
    );

    views[usize::from(gen_pins::PIN_SERVER)] = ModuleView::with_relaxed(
        ModState::Verified,
        vec!["ISource2ServerConfig::AllowSaveRestore (false in CS2)"],
    );
    let caps = control::capabilities_object_filtered(&views, 1, &|_| true);
    let want = format!(
        "\"{}\":{{\"state\":\"verified\",\"sha256\":\"{}\",\"relaxed\":[\"ISource2ServerConfig::AllowSaveRestore (false in CS2)\"]}}",
        gen_pins::PIN_MODULES[usize::from(gen_pins::PIN_SERVER)].name,
        gen_pins::PIN_MODULES[usize::from(gen_pins::PIN_SERVER)]
            .sha256
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    );
    assert!(caps.contains(&want), "the relaxed pin is reported: {caps}");
}

#[test]
fn gate_never_runs_for_an_unverified_need() {
    // The NEGATIVE: for a method whose needs module is not verified, gate() must not return
    // Run (the C answered -32017 PrerequisiteFailed for the same probe).
    for m in cs2_bridge::registry::METHODS {
        if m.implemented && m.needs != 0 {
            assert_ne!(
                gate_with(m.name, 0, true),
                Gate::Run,
                "{} needs modules the mask does not cover; the C answers -32017 (exec_request:5669-5698)",
                m.name
            );
        }
    }
    // ... and the failing module is the one the C's bits[] order names first: TIER0 precedes
    // PET in that order (mcp_bridge.c:5676-5677), so pet.apply (needs PET|TIER0) with nothing
    // verified answers tier0.dll
    assert_eq!(
        gate_with("pet.apply", 0, true),
        Gate::Prerequisite { module: "tier0.dll", pin: gen_pins::PIN_TIER0 }
    );
    assert_eq!(
        gate_with("pet.apply", cs2_bridge::registry::NEED_TIER0, true),
        Gate::Prerequisite { module: "pet.dll", pin: gen_pins::PIN_PET }
    );
}
