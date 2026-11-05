//! `cargo bridge testplugin`: build the coexistence fixture plugin (`crates/mm-testplugin`) for
//! both targets, stage the two libraries and gate their exports.
//!
//! ```text
//! target/testplugin/win64/mm_testplugin.dll
//! target/testplugin/linuxsteamrt64/libmm_testplugin.so
//! ```
//!
//! Both are release builds in the same memory-bounded form `cargo bridge build` runs for the
//! bridge ([`crate::build::release_build`]: no `--workspace`, `CARGO_TARGET_DIR` inside the
//! checkout, the address-space cap and the deadline, the environment's rustflags cleared), and
//! the bridge's export gate runs on each result: exactly one exported symbol, `CreateInterface`,
//! and no CRT import, or the command fails. The orchestrator installs the staged pair with a
//! test-only vdf; this command does not write one.

use anyhow::{bail, Result};

use crate::build::{cargo_target_dir, release_build, stage, TARGETS};
use crate::exports::{self, Gate};
use crate::root;

/// The command line of `cargo bridge testplugin`.
#[derive(clap::Args, Debug)]
pub struct Args {}

/// Runs the command; `Ok` is the exit code.
pub fn run(_args: Args) -> Result<u8> {
    let checkout = root::checkout();
    for target in TARGETS {
        release_build(&checkout, "testplugin", "mm-testplugin", target)?;
    }

    let target_dir = cargo_target_dir(&checkout);
    let out = checkout.join("target/testplugin");
    let pairs = [
        (target_dir.join(TARGETS[0]).join("release/mm_testplugin.dll"), out.join("win64/mm_testplugin.dll")),
        (target_dir.join(TARGETS[1]).join("release/libmm_testplugin.so"), out.join("linuxsteamrt64/libmm_testplugin.so")),
    ];
    for (built, _) in &pairs {
        if !built.is_file() {
            bail!("{} was not built", built.display());
        }
    }
    for (built, staged) in &pairs {
        stage(built, staged)?;
    }
    println!(
        "testplugin: staged {} and {}",
        pairs[0].1.display(),
        pairs[1].1.display()
    );

    let refused = pairs.iter().filter(|(_, staged)| exports::gate_file(staged, Gate::Bridge) != 0).count();
    if refused > 0 {
        eprintln!("testplugin: {refused} artefact(s) failed the export gate");
        return Ok(1);
    }
    Ok(0)
}
