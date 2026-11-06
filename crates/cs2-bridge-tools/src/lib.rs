//! `cargo bridge <command>`: the operator tooling of the bridge (`crates/cs2-bridge`), one
//! module per command. The binary (`src/main.rs`) only parses the command line and dispatches;
//! every command is a `run(Args) -> anyhow::Result<u8>` whose `Ok` value is the process exit
//! code, so a command that refuses with a documented code (install's `4`, metamod win64's `3`)
//! says so without an error.

pub mod build;
pub mod exports;
pub mod install;
pub mod metamod;
pub mod pins;
pub mod root;
pub mod runtime;
pub mod testplugin;
