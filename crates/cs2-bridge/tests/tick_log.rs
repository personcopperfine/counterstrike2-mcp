//! The tick's log rule, driven through the real vtable and the real log file.
//!
//! Five frames must write exactly three lines - "tick 1", "tick 2", "tick 4" - into the file
//! named by `CS2_BRIDGE_LOG`. This is the same file, prefix and rule the WP0.4 proof reads in
//! the Wine tools; only the process differs. It lives in its own test binary so nothing else in
//! the process touches `CS2_BRIDGE_LOG` or the frame counter.

use core::ffi::c_void;
use std::path::PathBuf;

use cs2_bridge::toolsys::{build_vtable, object, SLOT_RUN_FRAME};

/// The tick counts every frame and logs only the powers of two.
#[test]
fn five_frames_write_tick_1_then_2_then_4() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch/cs2-bridge-tick-5-frames.log");
    let _ = std::fs::remove_file(&path);
    std::env::set_var("CS2_BRIDGE_LOG", &path);

    let vt = build_vtable();
    // SAFETY: `SLOT_RUN_FRAME` holds `run_frame` (`*mut c_void -> ()`), called on the object
    // `CreateInterface` hands out - the same call the tools framework makes every frame.
    unsafe {
        let tick: unsafe extern "C" fn(*mut c_void) = core::mem::transmute(vt[SLOT_RUN_FRAME]);
        for _ in 0..5 {
            tick(object());
        }
    }

    std::env::remove_var("CS2_BRIDGE_LOG");
    // A missing file FAILS here: the tick's log line is the proof, not a nicety. The tag names
    // the roles the line came from: the tools object (`log::Role::Tool`).
    let text = std::fs::read_to_string(&path).expect("five frames must create the log file");
    assert_eq!(
        text,
        "[cs2-bridge tool] tick 1\n[cs2-bridge tool] tick 2\n[cs2-bridge tool] tick 4\n",
        "5 ticks log the powers of two only, each prefixed"
    );
    let _ = std::fs::remove_file(&path);
}
