//! Console access, three routes (least to most capable):
//! 1. `console.log` tail on the host (`-condebug -conclearlog`; `echo` does not reach it in `-tools`) [proven];
//! 2. VToolMessage `command` posts from inside the container (`vtmsend.exe`, ../idabins/worknotes/vtmsend)
//!    - tools runtime only [proven, used by run_vtest/vcmd.sh];
//! 3. bridge `console.exec {command}` -> engine2 IInputService cbuf; only in `-testscript` or `-tools` sessions,
//!    never during a level load [proven, guarded].
//!
//! The refusal guard (`cs2_mcp_common::console::refusal`) is route-independent: these commands end the
//! session or the process and must go through `stop_session` instead.
pub mod resolve;

pub mod log;

pub use log::{ConsoleError, ConsoleLog, Cursor, Tail, WaitError, WaitFrom, WaitMatch};
