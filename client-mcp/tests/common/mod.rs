//! Hermetic defaults shared by the test crates. Without them a config resolves the host's project
//! tree (DEFAULT_PROJECT_ROOT is baked from CARGO_MANIFEST_DIR: its console.log, compatdata, artifacts
//! and overviews), the real podman (running cs2mcp-* containers), the built capture layer under
//! target/, and the newest /run/user/<uid>/xauth_* cookie. The project, podman and layer defaults
//! point at paths that do not exist, so an unset flag means "nothing there". The X cookie default is
//! a file under this crate's CARGO_TARGET_TMPDIR, so a wine-tools launch finds a cookie without
//! reading the host's. A test that needs another value passes its own, which wins (see [`hermetic`]).
//! `--install-script` points at an inert stub, so a launch in a test never touches a real game tree.
#![allow(dead_code)]

const XAUTHORITY: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/hermetic-xauth");
/// Inert stand-in for `bridge/install-rs.sh` (the real one belongs to the game install).
const INSTALL_STUB: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/hermetic-install-rs.sh");

const HERMETIC: &[(&str, &str)] = &[
    ("--project-root", "/nonexistent/cs2mcp-test-project"),
    ("--podman", "/nonexistent/cs2mcp-test-podman"),
    ("--vk-layer-dir", "/nonexistent/cs2mcp-test-vklayer"),
    ("--xauthority", XAUTHORITY),
    ("--install-script", INSTALL_STUB),
];

/// `args` with each hermetic default prepended, unless `args` already passes that flag (by name or
/// as `--flag=value`). Clap rejects a repeated single-value flag, so the override has to be exact.
pub fn hermetic<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(args.len() + 2 * HERMETIC.len());
    for (flag, value) in HERMETIC {
        let given = args
            .iter()
            .any(|a| *a == *flag || a.starts_with(&format!("{flag}=")));
        if !given {
            if *flag == "--xauthority" && !std::path::Path::new(XAUTHORITY).exists() {
                std::fs::write(XAUTHORITY, "cookie").expect("hermetic cookie file");
            }
            if *flag == "--install-script" && !std::path::Path::new(INSTALL_STUB).exists() {
                std::fs::write(INSTALL_STUB, "#!/bin/bash\nexit 0\n")
                    .expect("hermetic install stub");
            }
            out.push(*flag);
            out.push(*value);
        }
    }
    out.extend_from_slice(args);
    out
}
