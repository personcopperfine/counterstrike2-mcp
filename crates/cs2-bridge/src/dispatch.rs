//! The bridge's request gate: the C's two dispatch sites over the ported method table, plus
//! the Rust-only "ported" list that says which of the C's implemented rows this build actually
//! serves.
//!
//! Two C functions ask the same table the same questions in different places:
//!
//! * `exec_request` (`mcp_bridge.c:5657-5698`, the main thread) - unknown ->
//!   `-32601`, `!impl` -> `-32011` with `blockedOn`, then the verified-mask check ->
//!   `-32017 PrerequisiteFailed`;
//! * `ipc_thread` (`:6603-6623`, the IPC thread) - unknown -> `-32601`, `!impl` -> `-32011`,
//!   `needs == 0` -> `-32601`, everything else takes a queue slot.
//!
//! [`decide`] is that shared decision. It keeps the C's order exactly - the `!impl` test still
//! precedes the needs test, so a documented-but-blocked row reports its own `blockedOn` - and
//! inserts one test the C has no counterpart for: `!ported`.
//!
//! # The `ported` list
//!
//! The C table's `implemented` flag means "the C bridge implements it". A Rust build in the
//! middle of the port implements fewer, and m3.0 implements none of the ops: [`PORTED`] lists
//! exactly the `control.*` rows. It is a separate list keyed by name rather than a field on
//! [`crate::registry::Method`] because that table is the frozen C table, held row for row by
//! the WP2.C differential (`tests/registry.rs`); adding a field would make the two disagree by
//! construction. A method that is in the table and implemented there but absent from
//! [`PORTED`] answers `-32011 NotImplemented` with [`NOT_PORTED_BLOCKED_ON`], and
//! [`crate::control`]'s filtered capabilities builder does not advertise it.
//!
//! The `ported` predicate is a parameter, not a global read, so a test (and M4's first
//! dispatch) can drive the gate with a different set without editing the table.

use crate::registry::{self, Gate};

/// The `blockedOn` of a row the C implements but this build does not serve yet.
pub const NOT_PORTED_BLOCKED_ON: &str = "rust bridge m3.0: not ported yet";

/// The `control.*` rows this build serves. Exactly the rows the IPC lane's fast path handles
/// (`mcp_bridge.c:6466-6601`); every op row is the C's and not yet the Rust bridge's.
pub static PORTED: &[&str] = &[
    "control.hello",
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "control.cancel",
    "control.shutdown",
];

/// Whether this build serves `name`: a [`PORTED`] `control.*` row, a row with a body in
/// [`crate::ops`], or this build's own control-plane row ([`crate::registry::RUST_METHODS`] /
/// [`crate::registry::RUST_OVERRIDES`], answered by its module on the lane - `metamod.plugins`,
/// `job.*`).
#[must_use]
pub fn is_ported(name: &str) -> bool {
    PORTED.contains(&name) || crate::ops::lookup(name).is_some()
        || crate::ops::metamod::serves(name)
        || crate::ops::job::serves(name)
}

/// What the bridge does with one method name under one verified mask - the union of the C's
/// `exec_request` head and its IPC gate chain.
#[derive(Debug, Eq, PartialEq)]
pub enum Decision {
    /// No such row: `-32601 MethodNotFound` at both C sites (`:5662`, `:6607-6610`).
    Unknown,
    /// The row is not servable now: `-32011 NotImplemented` with `{"blockedOn":...}`. The
    /// string is the C row's own `blockedOn` (`:6613`, `:5672`) or, for a C-implemented row
    /// this build has not ported, [`NOT_PORTED_BLOCKED_ON`].
    Blocked {
        /// The `blockedOn` text.
        blocked_on: &'static str,
    },
    /// Served, but a needs module is not verified: `-32017 PrerequisiteFailed` on the main
    /// thread (`exec_request:5669-5698`). The IPC thread queues the request and lets the tick
    /// answer it, exactly as the C does.
    Prerequisite {
        /// The first unmet need's module name, from the generated pins.
        module: &'static str,
        /// That module's pin index.
        pin: u8,
    },
    /// Every module is verified but the row needs a KHook `IKHook` and none is installed
    /// ([`crate::registry::Need_KHOOK`] is satisfied by `s2sdk::hook::api()`): `-32017`
    /// with the `blockedOn` text, see [`crate::control::khook_missing`].
    KhookMissing,
    /// Served and `needs == 0`: the `control.*` fast path's rows. The IPC lane answers these
    /// before [`decide`] runs; a row that reaches the C's IPC gate anyway answers `-32601`
    /// (`:6618-6621`), and `exec_request`'s tail does the same (`:6279`).
    FastPath,
    /// Served and its needs are met: a queue slot, then the method body. m3.0 has no such row
    /// (every ported row is `control.*`), so this arm is what M4 grows into.
    Queue,
}

/// The dispatch decision for `name` under `verified` (the C's `verified_mask`) and `ported`.
///
/// Order, matching `exec_request` (`:5657-5698`) with the Rust-only test inserted where the C's
/// dispatch would begin:
///
/// 1. no row -> [`Decision::Unknown`];
/// 2. `!implemented` -> [`Decision::Blocked`] with the row's `blockedOn` (`"unknown"` fallback,
///    `:5672`);
/// 3. not ported -> [`Decision::Blocked`] with [`NOT_PORTED_BLOCKED_ON`] - before the needs
///    check, so an unported row reports "not ported" rather than a module state that does not
///    matter yet;
/// 4. a needs module is unverified -> [`Decision::Prerequisite`], naming the first unmet need in
///    the C's `bits[]`/`mods[]` order (through [`registry::gate`], which is differentially
///    tested against the C);
/// 5. `needs == 0` -> [`Decision::FastPath`], else [`Decision::Queue`].
#[must_use]
pub fn decide(name: &str, verified: u32, ported: &dyn Fn(&str) -> bool) -> Decision {
    let Some(method) = registry::find(name) else {
        return Decision::Unknown;
    };
    if !method.implemented {
        return Decision::Blocked {
            blocked_on: method.blocked_on.unwrap_or("unknown"), // mcp_bridge.c:6613, :5672
        };
    }
    if !ported(name) {
        return Decision::Blocked {
            blocked_on: NOT_PORTED_BLOCKED_ON,
        };
    }
    match registry::gate(name, verified) {
        Gate::Prerequisite { module, pin } => Decision::Prerequisite { module, pin },
        Gate::KhookMissing => Decision::KhookMissing,
        Gate::Run if method.needs == 0 => Decision::FastPath,
        Gate::Run => Decision::Queue,
        // `find` answered above and the row is implemented, so `gate` cannot take its first two
        // arms; they are mapped rather than unreachable so a future edit of either function
        // stays total.
        Gate::Unknown => Decision::Unknown,
        Gate::Blocked { blocked_on } => Decision::Blocked { blocked_on },
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, is_ported, Decision, NOT_PORTED_BLOCKED_ON, PORTED};
    use crate::generated::pins;
    use crate::registry::{self, METHOD_COUNT};

    /// The self-test the WP gate asks for: every name in [`PORTED`] exists in the C table, is
    /// implemented there, and the list is exactly the `control.*` rows - the brief's "true only
    /// for `control.*`". A typo or a name the table does not have fails here, not at runtime.
    #[test]
    fn ported_is_a_subset_of_the_table_and_is_exactly_control() {
        for name in PORTED {
            let row = registry::find(name)
                .unwrap_or_else(|| panic!("ported lists `{name}`, absent from the method table"));
            assert!(row.implemented, "ported lists `{name}`, a blocked row");
        }
        for row in registry::METHODS {
            assert_eq!(
                is_ported(row.name),
                row.name.starts_with("control.")
                    || crate::ops::lookup(row.name).is_some()
                    || crate::ops::job::serves(row.name),
                "`{}`: served iff a control.* row, a row with an ops body or a lane-served job row",
                row.name
            );
        }
        // The table really is the 84 rows the differential pins; a truncated table must not
        // make the loop above vacuous.
        assert_eq!(registry::METHODS.len(), METHOD_COUNT);
    }

    #[test]
    fn unknown_is_method_not_found_and_blocked_keeps_its_own_reason() {
        assert_eq!(decide("no.such", 0, &is_ported), Decision::Unknown);
        // `doc.list` is the C's documented-but-blocked row (`mcp_bridge.c:1106`).
        assert_eq!(
            decide("doc.list", 0, &is_ported),
            Decision::Blocked {
                blocked_on: "bridge-protocol.md §7 doc.list (not implemented in M2.2)"
            }
        );
    }

    /// A C-implemented op answers "not ported" whatever the module state - the Rust-only test
    /// precedes the needs check, so a verified hammer does not change the answer. The row is
    /// named from the C table (`mesh.create`, implemented there), but the predicate is a
    /// synthetic closure: every M4 lane merge serves one more real row (mesh.* and the
    /// renders came in with their lanes, `input.set` with the hooks lane), so no real row
    /// stays unported for long and the example must not depend on the merged set.
    #[test]
    fn an_unported_row_is_not_ported_even_with_every_module_verified() {
        let not_ported = &|_name: &str| false;
        assert_eq!(
            decide("mesh.create", 0xfff, not_ported),
            Decision::Blocked {
                blocked_on: NOT_PORTED_BLOCKED_ON
            }
        );
        assert_eq!(
            decide("mesh.create", 0, not_ported),
            Decision::Blocked {
                blocked_on: NOT_PORTED_BLOCKED_ON
            }
        );
    }

    #[test]
    fn control_rows_are_the_fast_path() {
        for row in registry::METHODS.iter().filter(|row| row.name.starts_with("control.")) {
            assert_eq!(decide(row.name, 0, &is_ported), Decision::FastPath, "{}", row.name);
        }
    }

    /// The M4 shape, through the injected predicate: with the row "ported" the gate reaches the
    /// C's `-32017` arm, which names the first unmet need in `bits[]` order (tier0 before pet
    /// for `pet.apply`, `mcp_bridge.c:5676-5677`), and the queue once the needs are met.
    #[test]
    fn a_ported_row_reaches_the_prerequisite_and_queue_arms() {
        let all = |_: &str| true;
        assert_eq!(
            decide("pet.apply", 0, &all),
            Decision::Prerequisite {
                module: "tier0.dll",
                pin: pins::PIN_TIER0
            }
        );
        assert_eq!(
            decide("pet.apply", registry::NEED_TIER0, &all),
            Decision::Prerequisite {
                module: "pet.dll",
                pin: pins::PIN_PET
            }
        );
        assert_eq!(
            decide("pet.apply", registry::NEED_PET | registry::NEED_TIER0, &all),
            Decision::Queue
        );
        assert_eq!(decide("control.ping", 0, &all), Decision::FastPath);
    }
}
