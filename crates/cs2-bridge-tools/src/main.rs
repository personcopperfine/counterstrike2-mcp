//! `cs2-bridge-tools`, run as `cargo bridge <command>` (the alias in `.cargo/config.toml`).

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cs2_bridge_tools::{build, exports, install, metamod, pins, runtime, testplugin};

#[derive(Parser, Debug)]
#[command(name = "cargo bridge", bin_name = "cargo bridge", about = "Build, pin, install and run the cs2-mcp bridge")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Generate crates/cs2-bridge/src/generated/pins.rs from sigs/signatures (--check: refuse a stale one)
    Pins(pins::Args),
    /// Build the bridge and the Metamod loader for win64 + linux, stage target/bridge-rs, gate the exports
    Build(build::Args),
    /// Gate a built artefact's export table (the bridge: exactly CreateInterface)
    CheckExports(exports::Args),
    /// Install, uninstall, check or ensure the bridge + Metamod + launcher in a game tree
    Install(install::Args),
    /// The podman Wine tools runtime (up, open, cycle, status, logs, down)
    Runtime(runtime::Args),
    /// Build Metamod:Source in a container from the pinned metamod-source commit
    Metamod(metamod::Args),
    /// Build the crates/mm-testplugin coexistence fixture and stage it
    Testplugin(testplugin::Args),
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Pins(args) => pins::run(args),
        Command::Build(args) => build::run(args),
        Command::CheckExports(args) => exports::run(args),
        Command::Install(args) => install::run(args),
        Command::Runtime(args) => runtime::run(args),
        Command::Metamod(args) => metamod::run(args),
        Command::Testplugin(args) => testplugin::run(args),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("cargo bridge: {error:#}");
            ExitCode::from(1)
        }
    }
}
