//! `cs2mcp-launcher`: the desktop app over `cs2mcp-native` (README.md).
//!
//! It is also the AppImage's entry point, so it dispatches first:
//! `cs2mcp-launcher cs2mcp-native|client-mcp|hammer-mcp ...` runs that binary of the package
//! (`<AppImage> client-mcp ...` is what the MCP config names); `cs2mcp-launcher supervise <file>`
//! is the session supervisor, for a build without a `cs2mcp-native` next to it. No argument: the
//! window.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod worker;

use std::ffi::OsString;

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if let Some((bin, rest)) = cs2_native::selfexe::multicall_target(&args) {
        std::process::exit(exec(&bin, &rest));
    }
    if args.get(1).is_some_and(|a| a == "supervise") {
        let Some(sf) = args.get(2) else {
            eprintln!("usage: cs2mcp-launcher supervise <session file>");
            std::process::exit(1);
        };
        let r = cs2_native::session::supervise(std::path::Path::new(sf), &cs2_native::events::Events::stderr());
        if let Err(e) = r {
            eprintln!("[cs2mcp-native] error: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(a) = args.get(1) {
        eprintln!(
            "cs2mcp-launcher: unknown argument {:?}; without arguments it opens the window, or: {} <args> (the package's binaries)",
            a,
            cs2_native::selfexe::MULTICALL.join("|")
        );
        std::process::exit(1);
    }
    if let Err(e) = app::run() {
        eprintln!("cs2mcp-launcher: {e}");
        std::process::exit(1);
    }
}

/// Run a package binary in this process's place. The AppImage's environment stays for
/// `cs2mcp-native` (it finds `$APPIMAGE` for the supervisor and the MCP config) and is scrubbed
/// for the servers (their children must not see the AppImage's library paths).
fn exec(bin: &std::path::Path, args: &[OsString]) -> i32 {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args);
    if bin.file_stem().is_some_and(|s| s != "cs2mcp-native") {
        cs2_native::selfexe::scrub_appimage_env(&mut cmd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        eprintln!("cs2mcp-launcher: {}: {e}", bin.display());
        127
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("cs2mcp-launcher: {}: {e}", bin.display());
            127
        }
    }
}
