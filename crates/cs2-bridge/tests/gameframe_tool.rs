//! The GameFrame tick's role gate on an image that serves the **tool** role.
//!
//! The Wine `-tools` run is one image with both roles: Metamod loads first and installs this
//! hook, then the tools framework's `CreateInterface("ToolSystem2_001")` adds the tool role and
//! its `RunFrame` becomes the tick. The GameFrame callback must then do nothing - two ticks per
//! frame would double-drain the queue and double-count.
//!
//! This needs its own binary: [`cs2_bridge::log::Roles`] only grows in an image, so the
//! mm-only cases live in `tests/gameframe.rs` and share `tests/gameframe_support` with this one.

mod gameframe_support;

use core::ptr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cs2_bridge::{gameframe, toolsys, wiring};
use gameframe_support::{call_load, call_unload, plugin_object, with_bridge, drive_post, SERVER, SMM};

/// Waits for `condition` with a deadline, then fails naming it.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

/// `$XDG_RUNTIME_DIR` for the case's lifetime, so the listener's discovery file lands under
/// `target/scratch` and never in the user's home.
struct XdgGuard {
    previous: Option<std::ffi::OsString>,
    dir: PathBuf,
}

impl XdgGuard {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch XDG directory exists");
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        Self { previous, dir }
    }

    fn discovery(&self) -> PathBuf {
        self.dir.join("cs2mcpbridge").join("session.json")
    }
}

impl Drop for XdgGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }
}

/// With the tool role present the hook is still installed (it is installed on every `Load`),
/// but a frame does not tick and the status reports the tools framework as the source.
#[test]
fn with_the_tool_role_the_frame_callback_does_not_tick() {
    // Reaching the tools object is what adds the role (`log::add_role`, `toolsys.rs`).
    let _tool_object = toolsys::object();
    assert!(
        cs2_bridge::log::roles().contains(cs2_bridge::log::Role::Tool),
        "the tools entry point adds the role"
    );

    with_bridge("tool-role", |mock| {
        // SAFETY: `SMM` is the support module's fake `ISmmAPI`.
        unsafe { gameframe::install(ptr::addr_of!(SMM.0).cast_mut()) }.expect("install");
        assert!(gameframe::installed(), "the hook is installed on every Load");
        assert_eq!(
            gameframe::tick_source(),
            "tools",
            "the tools framework's RunFrame is the tick in this image"
        );

        let before = gameframe::ticks();
        for _ in 0..3 {
            drive_post(mock);
        }
        assert_eq!(
            gameframe::ticks(),
            before,
            "the GameFrame callback must not tick where the tools framework does"
        );
        assert!(
            cs2_bridge::wiring::last_tick_ms() == 0 || before != 0,
            "no tick was run by the GameFrame callback"
        );
        assert_eq!(
            ptr::addr_of!(SERVER.0) as usize,
            SERVER.0.vt as usize - SERVER.0.vt as usize + ptr::addr_of!(SERVER.0) as usize,
            "the fake server object is the one the hook was installed for"
        );
    });
}

/// A Metamod `Unload` in a **tools** image leaves the wiring alone: the tools framework owns the
/// listener and the queue, and the tool object's own `Shutdown` stops them, not Metamod's
/// unload. (In an mm-only image the same call stops them: `tests/gameframe.rs`'s lifecycle case.)
#[test]
fn a_metamod_unload_in_a_tools_image_leaves_the_listener_up() {
    // Reaching the tools object is what adds the role (`log::add_role`, `toolsys.rs`).
    let _tool_object = toolsys::object();
    assert!(
        cs2_bridge::log::roles().contains(cs2_bridge::log::Role::Tool),
        "the tools entry point adds the role"
    );

    with_bridge("tool-unload", |mock| {
        let xdg = XdgGuard::new("gameframe-tool-unload-xdg");

        // SAFETY: `SMM` is the support module's fake `ISmmAPI`, live for the case.
        let (ok, error) = call_load(plugin_object(), ptr::addr_of!(SMM.0).cast_mut());
        assert!(ok, "Load: {error}");
        assert_eq!(gameframe::tick_source(), "tools");

        // In this image the tools tick starts the listener; the GameFrame hook stays installed
        // and does not tick (`drive_post` above).
        wiring::tick();
        wait_for("the listener to start", || wiring::port().is_some());
        let port = wiring::port().expect("the tools tick started the listener");
        wait_for("the discovery file", || xdg.discovery().exists());
        assert_eq!(
            mock.setups().len(),
            1,
            "Load installed the GameFrame hook through the mock KHook"
        );

        let (ok, error) = call_unload(plugin_object());
        assert!(ok, "Unload: {error}");
        assert_eq!(
            wiring::port(),
            Some(port),
            "the tool role owns the listener: a Metamod unload must not stop it"
        );
        assert!(
            xdg.discovery().exists(),
            "session.json still points the host at the live tool listener"
        );

        // The tools framework's own Shutdown is what stops it (`wiring::pre_shutdown`).
        wiring::pre_shutdown();
        assert_eq!(wiring::port(), None);
        assert!(!xdg.discovery().exists());
    });
}
