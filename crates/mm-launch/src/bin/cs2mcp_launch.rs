//! `cs2mcp_launch.exe` - the launcher (CONTRACT "Runtime").
//!
//! The launcher's steps and its failure contract live in [`launcher`]; on Linux this package
//! still builds the same bin (the workspace builds every member for the Linux target), so the
//! Linux binary says what it is instead of pretending to start a game.

#![deny(unsafe_op_in_unsafe_fn)]

// The launcher shares the pure helpers with the loader, and this package builds the library as a
// `cdylib` only (CONTRACT: `crate-type = ["cdylib"]`), which a bin cannot link against. So the
// modules are included by path: one source file each, compiled into the bin as well as into the
// library. `paths`/`cmdline`/`log` are `cfg`-free on purpose, so the same tests cover both.
//
// `allow(dead_code)` on `paths`/`log`: the launcher uses one derivation from `paths` and one tag
// from `log`, and the rest of each module belongs to the other half of the crate (the DLL and the
// Linux interposer). The alternative - duplicating the rules in the bin - is exactly what would
// let the two halves drift apart.
#[cfg(windows)]
#[path = "../cmdline.rs"]
mod cmdline;
#[cfg(windows)]
#[path = "../launcher.rs"]
mod launcher;
#[cfg(windows)]
#[allow(dead_code)]
#[path = "../log.rs"]
mod log;
#[cfg(windows)]
#[allow(dead_code)]
#[path = "../paths.rs"]
mod paths;

#[cfg(windows)]
fn main() {
    std::process::exit(launcher::run());
}

#[cfg(not(windows))]
fn main() {
    // CONTRACT: "on Linux the bin is a stub that prints 'windows only' and exits 2". Linux loads
    // the interposer `libcs2mcp_mmload.so` through `LD_PRELOAD`; no launcher runs there.
    eprintln!(
        "cs2mcp_launch: windows only (the Linux loader is libcs2mcp_mmload.so, LD_PRELOADed on the cs2.sh line)"
    );
    std::process::exit(2);
}
