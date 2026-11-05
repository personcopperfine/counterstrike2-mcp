//! Wire constants and the few typed shapes the client needs. Unknown members are ignored.
pub const PROTOCOL: u32 = 1;
pub const DEFAULT_DEADLINE_MS: u64 = 30_000;
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_REPLY_GRACE_MS: u64 = 5_000;
pub const DEFAULT_BLOCKED_AGE_MS: u64 = 10_000;
/// Protocol §2: clients must not assume more than the advertised limit (m2.8: 1 MiB). This is
/// the ceiling before hello, lowered to the advertised value afterwards.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// `doc.active` reasons that mean "Hammer is not usable yet" (`mcp_bridge.c active_doc`).
pub const HAMMER_NOT_LOADED_REASONS: &[&str] = &[
    "hammer_not_loaded",
    "hammer_unavailable",
    "hammer_verifying",
    "tf2_not_loaded",
    "tf2_verifying",
];
/// `doc.active` reasons that mean the fail-closed identity gate tripped.
pub const MISMATCH_REASONS: &[&str] = &["hammer_mismatch", "tf2_mismatch"];
