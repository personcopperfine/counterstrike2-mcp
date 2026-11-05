//! The control reply builders' differential against the C: for every scenario the brief
//! names (all modules not_loaded, all verified, mixed - hammer mismatch, toolframework2
//! verifying - and a bumped revision), the generator runs the C builders extracted verbatim
//! from the frozen `bridge/mcp_bridge.c` and this port must emit the same bytes.
//!
//! Records compared, per scenario:
//! * `CAPS` - `control.capabilities` (`mcp_bridge.c:6554-6558`),
//! * `HELLO` - the `control.hello` ok reply with and without an id (`:6478-6486`),
//! * `PING` - `control.ping`, three input shapes (`:6497-6529`),
//! * `STATUS` - `control.status`, full and empty queue (`:6530-6553`),
//! * `VERSION` - `control.version` (`:6559-6578`),
//! * `IPCGATE` - the gate arms' error objects for unknown and blocked methods
//!   (`:6601-6621`),
//! * `EXECGATE` - `exec_request`'s head through the `-32017 PrerequisiteFailed` object
//!   (`:5657-5698`); `-` (empty) where the gate passed and dispatch begins - the Rust side
//!   must see [`Gate::Run`] there, and MUST NOT see it where a needs module is unverified.
//!
//! The negative: one flipped hex digit in the C output must break the comparison, and the
//! gate must refuse `Run` for a method whose needs module is not verified.

mod common;

use cs2_bridge::control::{self, PingInput, QueueEntry, StatusInput};
use cs2_bridge::generated::pins as gen_pins;
use cs2_bridge::registry::{gate_c_with, gate_with, verified_mask, Gate, ModState, ModuleView};
use std::sync::OnceLock;

/// The generator's records, cached across the tests in this binary.
fn generator_lines() -> &'static Vec<String> {
    static LINES: OnceLock<Vec<String>> = OnceLock::new();
    LINES.get_or_init(common::run_generator)
}

/// The pin-order module states the generator's scenario sets (registry_gen.c `run_scenario`).
/// Pin order: hammer, toolframework2, scenesystem, engine2, rendersystemvulkan, client,
/// server, tier0, schemasystem, soundsystem, modeldoc_editor, pet.
fn scenario_modules(scenario: &str) -> Vec<ModuleView> {
    let verified = ModState::Verified;
    let states: &[ModState] = match scenario {
        "not_loaded" | "rev2" => &[ModState::NotLoaded; 12],
        "verified" => &[verified; 12],
        "mixed" => &[
            ModState::Mismatch,   // hammer.dll
            ModState::Verifying,  // toolframework2.dll
            verified,             // scenesystem.dll
            verified,             // engine2.dll
            verified,             // rendersystemvulkan.dll
            verified,             // client.dll
            verified,             // server.dll
            verified,             // tier0.dll
            verified,             // schemasystem.dll
            ModState::NotLoaded,  // soundsystem.dll
            verified,             // modeldoc_editor.dll
            ModState::NotLoaded,  // pet.dll
        ],
        other => panic!("the generator prints no scenario named `{other}`"),
    };
    assert_eq!(states.len(), gen_pins::PIN_MODULE_COUNT);
    states.iter().map(|&s| ModuleView::new(s)).collect()
}

/// The revision the generator's scenario sets (registry_gen.c `run_scenario`).
fn scenario_revision(scenario: &str) -> u64 {
    match scenario {
        "not_loaded" => 1,
        "verified" => 5,
        "mixed" => 7,
        "rev2" => 2,
        other => panic!("the generator prints no scenario named `{other}`"),
    }
}

/// `control.ping`'s three input shapes, exactly the generator's PING variants
/// (registry_gen.c `run_scenario`).
fn ping_input<'a>(variant: &str, modules: &'a [ModuleView], revision: u64) -> PingInput<'a> {
    match variant {
        "A" => PingInput {
            now: 10000, start_tick: 9000, last_tick: 9950, inflight_start: 0,
            queue_depth: 0, modules, revision, shutting_down: false,
        },
        "B" => PingInput {
            now: 20000, start_tick: 10000, last_tick: 20500, inflight_start: 19000,
            queue_depth: 3, modules, revision, shutting_down: false,
        },
        "noid" => PingInput {
            now: 500, start_tick: 500, last_tick: 0, inflight_start: 240,
            queue_depth: 1, modules, revision, shutting_down: false,
        },
        other => panic!("the generator prints no PING variant `{other}`"),
    }
}

/// `control.status`'s two input shapes, exactly the generator's STATUS variants.
fn status_input(variant: &str, revision: u64) -> StatusInput<'static> {
    const ENTRIES: &[QueueEntry<'static>] = &[
        QueueEntry { id: "id-1", method: "doc.active", received_at: 19000 },
        QueueEntry { id: "id-2", method: "pet.apply", received_at: 19999 },
    ];
    match variant {
        "A" => StatusInput {
            now: 20000, queue: ENTRIES, listener: true, inflight_start: 0,
            revision, shutting_down: false, jobs: &[],
        },
        "B" => StatusInput {
            now: 20000, queue: &[], listener: false, inflight_start: 42,
            revision, shutting_down: true, jobs: &[],
        },
        other => panic!("the generator prints no STATUS variant `{other}`"),
    }
}

/// The Rust reply for one record tag, over the scenario's inputs.
fn rust_reply(tag: &str, name: Option<&str>, scenario: &str) -> Vec<u8> {
    let mods = scenario_modules(scenario);
    let rev = scenario_revision(scenario);
    let variant = name.expect("PING/STATUS/HELLO records carry a variant name");
    let method = variant; // the same field carries the method for the gate records
    match tag {
        "CAPS" => control::capabilities(Some("caps-id-1"), &mods, rev).into_bytes(),
        "HELLO" if variant == "noid" => control::hello(None, &mods, rev).into_bytes(),
        "HELLO" => control::hello(Some("hello-id-1"), &mods, rev).into_bytes(),
        "PING" => control::ping(
            if variant == "noid" { None } else { Some("ping-id-1") },
            &ping_input(variant, &mods, rev),
        )
        .into_bytes(),
        "STATUS" => control::status(Some("status-id-1"), &status_input(variant, rev)).into_bytes(),
        "VERSION" => control::version(Some("version-id-1"), &mods).into_bytes(),
        "IPCGATE" => ipc_gate_reply(method, &mods),
        "EXECGATE" => exec_gate_reply(method, &mods),
        other => panic!("no Rust builder for record {other}/{variant}"),
    }
}

/// The expected ipc_thread gate arm answer for one probe (mcp_bridge.c:6601-6623): the C
/// arm answers -32601 for an unknown name and for a needs==0 row that slipped past the fast
/// path, -32011 for a blocked row, and nothing (queued) otherwise.
///
/// The gate is driven with `khook_present = true` (`gate_c_with`): the C table has no
/// `NEED_KHOOK`, so every probe of the differential is a host where KHook *is* installed and
/// the Rust-only bit is satisfied - the KHook-absent steering is the Rust build's own case and
/// is tested in `tests/ops_hooks.rs`, not against the C.
fn ipc_gate_reply(method: &str, mods: &[ModuleView]) -> Vec<u8> {
    let mask = verified_mask(mods);
    match gate_c_with(method, mask, true) {
        Gate::Unknown => control::not_found(Some("gate-id-1")).into_bytes(),
        Gate::Blocked { blocked_on } => {
            control::not_implemented(Some("gate-id-1"), blocked_on).into_bytes()
        }
        // control.ping is the one probe with needs == 0: when such a row reaches the gate
        // arm the C answers -32601 (mcp_bridge.c:6618-6621). In the shipped table every
        // needs==0 implemented row IS control.* and the fast path replies first (the PING
        // differential proves that reply); the arm's answer is what is compared here.
        Gate::Run if method == "control.ping" => control::not_found(Some("gate-id-1")).into_bytes(),
        Gate::Run => panic!("the ipc gate arm must not answer a queued method: {method}"),
        Gate::Prerequisite { .. } | Gate::KhookMissing => {
            panic!("the ipc gate arm never reaches the mask check: {method}")
        }
    }
}

/// The expected exec_request head answer for one probe (mcp_bridge.c:5657-5698): -32601 for
/// an unknown name, -32011 for a blocked row, -32017 with the first unmet need's module and
/// state, and NOTHING where the gate passed (dispatch begins) - [`Gate::Run`].
fn exec_gate_reply(method: &str, mods: &[ModuleView]) -> Vec<u8> {
    let mask = verified_mask(mods);
    match gate_c_with(method, mask, true) {
        Gate::Unknown => control::not_found(Some("exec-id-1")).into_bytes(),
        Gate::Blocked { blocked_on } => {
            control::not_implemented(Some("exec-id-1"), blocked_on).into_bytes()
        }
        Gate::Prerequisite { module, pin } => {
            control::prerequisite_failed(Some("exec-id-1"), module, mods[usize::from(pin)].state)
                .into_bytes()
        }
        // The C has no KHook gate: on this differential's hosts KHook is installed, so this
        // arm is unreachable (and would be a harness bug, not a C mismatch).
        Gate::KhookMissing => panic!("KHook is present in every differential scenario: {method}"),
        Gate::Run => Vec::new(),
    }
}

/// The Rust-only `"relaxed"` member of a module object: present only when the KHook-aware
/// identity check relaxed a pin for that module, absent (not `[]`) otherwise - which is what
/// keeps the bytes identical to the C for every module with nothing relaxed (the differential
/// above compares them).
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
fn replies_match_the_c_scenario_by_scenario() {
    let mut scenario = String::new();
    let mut compared = 0usize;
    for line in generator_lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "SCENARIO" => scenario = f[1].to_string(),
            "METHOD" | "METHODCOUNT" => {} // the registry test's records
            "CAPS" | "HELLO" | "PING" | "STATUS" | "VERSION" | "IPCGATE" | "EXECGATE" => {
                let name = f.get(1).copied();
                let hex = f.last().expect("a hex field");
                let want = common::unhex(hex);
                let tag = f[0];
                assert_eq!(
                    rust_reply(tag, name, &scenario),
                    want,
                    "{tag}/{name:?} differs from the C in scenario `{scenario}`"
                );
                compared += 1;
            }
            other => panic!("the generator printed an unknown record `{other}`"),
        }
    }
    // not_loaded + verified + mixed + rev2, each with 1 CAPS + 2 HELLO + 3 PING + 2 STATUS
    // + 1 VERSION + 15 IPCGATE + 24 EXECGATE
    assert_eq!(compared, 4 * (1 + 2 + 3 + 2 + 1 + 15 + 24), "every record must be compared");
}

#[test]
fn a_corrupted_c_byte_is_detected() {
    // The negative half: flipping one hex digit of the C's CAPS output must break the
    // comparison - the comparator compares, the scenarios are not vacuously equal.
    let mut scenario = String::new();
    for line in generator_lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f[0] == "SCENARIO" {
            scenario = f[1].to_string();
        }
        if f[0] == "CAPS" && scenario == "verified" {
            let hex = f.last().expect("a hex field");
            let mut corrupted = hex.to_string();
            let mid = corrupted.len() / 2;
            let flipped = if corrupted.as_bytes()[mid] == b'0' { "1" } else { "0" };
            corrupted.replace_range(mid..mid + 1, flipped);
            assert_ne!(
                common::unhex(&corrupted),
                rust_reply("CAPS", Some("-"), &scenario),
                "a flipped byte in the C output must be detected"
            );
            return;
        }
    }
    panic!("the generator printed no CAPS record for the verified scenario");
}

#[test]
fn gate_never_runs_for_an_unverified_need() {
    // The brief's NEGATIVE, on the Rust side: for a method whose needs module is not
    // verified, gate() must not return Run - and the differential above already pins the
    // C's exact error object for the same probe (EXECGATE doc.active / pet.load / ...).
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
