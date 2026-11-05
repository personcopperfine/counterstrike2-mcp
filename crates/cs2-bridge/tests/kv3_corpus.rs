//! The KV3 gate over the installed workshop content tree: the Rust twin of
//! `bridge/test_kv3_corpus.sh`. It needs the Wine tools install, so it is its own test target with
//! `test = false` in Cargo.toml: `cargo test --workspace` (no game needed, AGENTS.md) does not run
//! it; run it explicitly with `cargo test -p cs2-bridge --test kv3_corpus`. A missing tree fails
//! loudly (`CS2_CONTENT` overrides the path).

use std::path::{Path, PathBuf};

use cs2_bridge::kv3_check::{kv3_check, vpcf_check};

/// The corpus script's walk, in Rust: every kv3-header file and every `.vpcf` under the installed
/// workshop content must be accepted, and the counts must be the C runner's.
///
/// `bridge/test_kv3_corpus.sh` printed `109 kv3 files checked, 4 vpcf files checked, 0 refusals`
/// on 2026-10-19; the counts follow the user's content, so only the floor and 0 refusals are asserted.
#[test]
fn corpus_matches_the_c_runner() {
    let content = content_dir();
    let mut kv3_files = 0usize;
    let mut vpcf_files = 0usize;
    let mut refusals: Vec<String> = Vec::new();
    walk(&content, &mut |path| {
        let bytes = std::fs::read(path).unwrap_or_else(|error| {
            panic!("the corpus walk must read {}: {error}", path.display())
        });
        let is_vpcf = path.extension().is_some_and(|extension| extension == "vpcf");
        let is_kv3 = starts_with_kv3_header(&bytes);
        if !is_kv3 && !is_vpcf {
            return;
        }
        if is_kv3 {
            kv3_files += 1;
            if let Err(refusal) = kv3_check(&bytes) {
                refusals.push(format!("REFUSE kv3  {}: {refusal}", path.display()));
            }
        }
        if is_vpcf {
            vpcf_files += 1;
            if let Err(refusal) = vpcf_check(&bytes) {
                refusals.push(format!("REFUSE vpcf {}: {refusal}", path.display()));
            }
        }
    });
    assert!(refusals.is_empty(), "the corpus must be accepted:\n{}", refusals.join("\n"));
    assert!(vpcf_files >= 1, "the corpus walk found no .vpcf file (the C runner found 4 on 2026-10-19)");
    assert!(kv3_files >= 50, "the corpus walk was truncated: only {kv3_files} kv3 files");
}

/// The installed workshop content tree; a missing tree fails loudly, like the C script's
/// `exit 2`. `CS2_CONTENT` overrides the default, as it does for `bridge/test_kv3_corpus.sh`.
fn content_dir() -> PathBuf {
    let content = PathBuf::from(
        std::env::var("CS2_CONTENT")
            .unwrap_or_else(|_| {
                let root = std::env::var("CS2_MCP_PROJECT_ROOT")
                    .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string());
                format!("{root}/workshop/run/app/content")
            }),
    );
    assert!(
        content.is_dir(),
        "the KV3 corpus test needs the installed content tree at {} (bridge/test_kv3_corpus.sh \
         has the same requirement); set CS2_CONTENT",
        content.display()
    );
    content
}

/// The C runner's `starts_with_kv3`: a BOM, then `<!-- kv3` with the space.
fn starts_with_kv3_header(bytes: &[u8]) -> bool {
    let offset = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    bytes.len() >= offset + 8 && &bytes[offset..offset + 8] == b"<!-- kv3"
}

/// The corpus script's walk: `_toolsautosave/` is skipped entirely and `mcp_*` *files* (the
/// bridge/MCP test scratch files) are skipped, but a `mcp_*` directory is walked - the C script
/// only tests the name for a regular file, and the `mcp_probe/` directory is where the `.vpcf`
/// fixtures live. Regular files only.
fn walk(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("the corpus walk must open {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("a readable directory entry");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "." || name == ".." || name == "_toolsautosave" {
            continue;
        }
        let path = entry.path();
        let meta = entry
            .metadata()
            .unwrap_or_else(|error| panic!("the corpus walk must stat {}: {error}", path.display()));
        if meta.is_dir() {
            walk(&path, visit);
        } else if meta.is_file() && !name.starts_with("mcp_") {
            visit(&path);
        }
    }
}
