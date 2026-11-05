//! The `-insecure` gate: the bridge never starts in a VAC-secured client session.
//!
//! Injected code must not run where VAC is on (crates/cs2-native README "VAC"). Every launch path
//! this repo has passes `-insecure` (`cargo bridge runtime`, the client-mcp runtime, `cs2mcp-native`),
//! and `cs2mcp_launch.exe` refuses without it. What those checks do not cover is the tool-plugin
//! entry in `sdkenginetools.txt`: while `cs2mcp-native` has an install prepared, a `-tools`
//! session the user starts from the Steam UI loads this image without `-insecure`
//! (crates/cs2-native FIXME 1). So both lifecycle starts ask this gate first: the tools role's
//! `Init` slot ([`crate::toolsys`]) and the Metamod `Load` ([`crate::plugin`]).
//!
//! `-dedicated` counts too: a dedicated server's own Metamod is the normal way server plugins run,
//! and VAC there is the server's setting, not a client's account.
use std::sync::atomic::{AtomicBool, Ordering};

static ALLOW_FOR_TESTS: AtomicBool = AtomicBool::new(false);

/// The refusal, for the bridge log and the Metamod error buffer.
pub const REFUSAL: &str = "cs2-mcp bridge: refusing to start without -insecure on the command line (a VAC-secured session; launch with cs2mcp-native, cargo bridge runtime or -insecure)";

/// Does this process run with `-insecure` (or `-dedicated`)? A unit-test build always answers yes.
pub fn insecure_session() -> bool {
    if cfg!(test) || ALLOW_FOR_TESTS.load(Ordering::Acquire) {
        return true;
    }
    crate::ops::game::cmdline_has("-insecure") || crate::ops::game::cmdline_has("-dedicated")
}

/// Integration tests drive `Load`/`Init` from a test binary whose command line has neither flag.
/// Rust callers only (not exported from the DLL); the game cannot reach it.
#[doc(hidden)]
pub fn allow_secure_for_tests() {
    ALLOW_FOR_TESTS.store(true, Ordering::Release);
}
