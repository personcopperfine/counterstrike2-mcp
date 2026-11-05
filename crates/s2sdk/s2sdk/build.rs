//! Compiles the upstream Source 2 SDK `.proto` files into prost Rust sources.
//!
//! Runs only when the `proto` feature is enabled (`CARGO_FEATURE_PROTO` is set); the
//! upstream SDK tree (`$S2SDK_DIR`, default: the `crates/s2sdk/upstream` git submodule of this
//! repo) is read-only — protos are compiled into `OUT_DIR`.
//!
//! `protoc` comes from the upstream SDK itself (`devtools/bin/linux/protoc`, 3.21.8), so
//! no system protobuf install is needed.

use std::env;
use std::path::PathBuf;

/// Proto include roots inside `$S2SDK_DIR`. The files import by bare filename and have
/// no `package` statements, so every directory that owns protos is on the include path
/// and everything is generated into one module (`_.rs`).
const INCLUDE_DIRS: &[&str] = &[
    "common",
    "game/shared",
    "game/shared/cs",
    "game/shared/cstrike15",
    "gcsdk",
    "thirdparty/protobuf-3.21.8/src", // google/protobuf/descriptor.proto
];

/// Directories (relative to `$S2SDK_DIR`) whose `*.proto` files are all compiled. New upstream
/// protos in them are picked up automatically; only [`EXCLUDED_PROTOS`] is maintained by hand.
const PROTO_DIRS: &[&str] = &[
    "common",
    "game/shared",
    "game/shared/cs",
    "game/shared/cstrike15",
    "gcsdk",
];

/// Protos left out, with the reason. `base_gcmessages.proto` defines ~80 top-level names
/// (e.g. `CMsgGCStorePurchaseInit`, `CSOEconItem`) identical to its CS variant
/// `base_gcmessages_csgo.proto`, which we keep; nothing else imports it.
const EXCLUDED_PROTOS: &[&str] = &["base_gcmessages.proto"];

/// Every `*.proto` basename in [`PROTO_DIRS`] minus [`EXCLUDED_PROTOS`], sorted, with its path.
///
/// Roots are passed to protoc by bare filename, not by path: the files import each other by
/// bare filename and protoc canonicalizes a file through the first include directory that has
/// it, so `game/shared/x.proto` plus an `import "x.proto"` would parse the file twice
/// ("already defined"). That needs unique basenames across the directories, checked here.
fn discover_protos(sdk: &std::path::Path) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for dir in PROTO_DIRS {
        let dir = sdk.join(dir);
        // a new file in the directory must re-run the build script
        println!("cargo:rerun-if-changed={}", dir.display());
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("readable directory entry").path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if !name.ends_with(".proto") || EXCLUDED_PROTOS.contains(&name.as_str()) {
                continue;
            }
            if let Some((_, other)) = found.iter().find(|(n, _)| *n == name) {
                panic!("proto basename {name} exists twice ({} and {}); protoc resolves imports by bare name, so exclude one in EXCLUDED_PROTOS", other.display(), path.display());
            }
            found.push((name, path));
        }
    }
    found.sort();
    found
}

/// Find the upstream s2sdk checkout: `$S2SDK_DIR` if set, else the `crates/s2sdk/upstream` git
/// submodule (`<workspace>/s2sdk/upstream`, i.e. `../upstream` from this crate).
fn find_sdk_dir() -> PathBuf {
    if let Some(dir) = env::var_os("S2SDK_DIR") {
        let dir = PathBuf::from(dir);
        assert!(
            dir.is_dir(),
            "S2SDK_DIR={:?} does not exist (expecting the s2sdk checkout)",
            dir
        );
        return dir;
    }
    let manifest = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR is set by cargo for build scripts"),
    );
    let candidate = manifest.join("../upstream");
    assert!(
        candidate.join("public").is_dir(),
        "cannot locate the Source 2 SDK checkout: run `git submodule update --init crates/s2sdk/upstream` \
         or set S2SDK_DIR to an s2sdk checkout"
    );
    candidate
}

fn main() {
    println!("cargo:rerun-if-env-changed=S2SDK_DIR");
    if env::var_os("CARGO_FEATURE_PROTO").is_none() {
        return;
    }

    let sdk = find_sdk_dir();
    let protoc = sdk.join("devtools/bin/linux/protoc");
    assert!(
        protoc.is_file(),
        "protoc not found at {} (upstream devtools binary, 3.21.8)",
        protoc.display()
    );

    let roots = discover_protos(&sdk);
    for (_, path) in &roots {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let mut config = prost_build::Config::new();
    config.protoc_executable(&protoc);
    let protos: Vec<&str> = roots.iter().map(|(name, _)| name.as_str()).collect();
    let includes: Vec<PathBuf> = INCLUDE_DIRS.iter().map(|f| sdk.join(f)).collect();

    config
        .compile_protos(&protos, &includes)
        .expect("failed to compile upstream s2sdk protos");
}
