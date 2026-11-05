//! `mm-launch`: gameinfo-free Metamod loading (KHook `DESIGN.md` §14 follow-up,
//! `target/DESIGN.md` §1). One crate, two artefacts per platform, all named by
//! `target/CONTRACT.md`:
//!
//! * **the loader** - `cs2mcp_mmload.dll` (Wine) / `libcs2mcp_mmload.so` (Linux): puts Metamod's
//!   `server.dll` in front of the engine's own `server.dll` at the one moment the engine asks for
//!   it, so the engine's `gameinfo.gi` needs no `SearchPaths` line at all;
//! * **the launcher** - `cs2mcp_launch.exe` (`src/bin/`): starts `cs2.exe` suspended, injects the
//!   DLL with `LoadLibraryW` on a remote thread and resumes it, passing Metamod's loader path in
//!   `CS2MCP_MM_LOADER`.
//!
//! # Why a hook and not gameinfo
//!
//! The previous layout edited `gameinfo.gi` to put a Metamod directory on the engine's search
//! path. That is a game file (Steam updates undo it, and `ensure` re-applied it every boot) and,
//! on the `-tools` side, the search path itself was what made the asset system abort. The spike
//! (`target/DESIGN.md` §0) proved the alternative live on all four paths: the engine asks for its
//! server image by absolute path, exactly once, through `kernel32!LoadLibraryExW` on Wine and
//! through `dlopen` on Linux. Answering that one request with Metamod's loader gets a normal
//! Metamod boot with every game file pristine - Metamod's own loader then finds the *real* game
//! image itself from the unmodified gameinfo (it skips its own path).
//!
//! # Beyond the two artefacts
//!
//! * [`paths`] - the redirect rules (which path is the engine's image, where Metamod sits) and the
//!   Windows loader-path derivation. Pure, `cfg`-free, unit-tested on the host.
//! * [`cmdline`] - the raw-command-line split the launcher needs to stay a transparent stand-in
//!   for `cs2.exe`. Pure, `cfg`-free, unit-tested on the host.
//! * [`log`] - the `[cs2mcp-*]` stderr lines, written without stdio locks (the DLL logs from
//!   `DllMain`).
//!
//! # Fail-closed
//!
//! Every failure is loud and stops the boot instead of continuing without Metamod: the DLL returns
//! `FALSE` from `DllMain` (the launcher then terminates the suspended `cs2.exe` and exits 3), the
//! Linux interposer logs and `abort()`s, and the launcher refuses to start at all if `cs2.exe`, the
//! DLL or Metamod's loader is missing. A silent Metamod-less session is what the old design was
//! trying to avoid; losing the game start instead is the accepted trade (DESIGN §4).
//!
//! # Exports
//!
//! The loader is not a plugin: on Windows it exports nothing the engine could call (only
//! `DllMain`, which the loader finds through the PE entry point), on Linux exactly the one
//! interposed `dlopen`. `cargo bridge check-exports --loader` is the gate, run by `cargo bridge
//! build` on the staged files.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod cmdline;
pub mod log;
pub mod paths;

#[cfg(windows)]
mod dll;

// The `dlopen` interposer is a `#[no_mangle]` `dlopen`. In a test build that symbol would sit in
// the test binary's dynamic symbol table and interpose `dlopen` for the test harness itself (the
// linker prefers the executable's definition), so `cargo test -p mm-launch` would run its unit
// tests against our stub - a trap the spike already had to think about. The interposer is
// therefore compiled only for the real (non-test) library; everything worth testing is in the
// `cfg`-free modules above, which the tests link normally. `cargo check`/`clippy` on the native
// target still compile this module (the lib target is also built without `--test`), so it is
// linted and type-checked in every gate that matters.
#[cfg(all(unix, not(test)))]
mod preload;
