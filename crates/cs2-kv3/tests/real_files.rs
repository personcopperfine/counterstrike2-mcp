//! Byte-identical round trips on the **real** sample files (read-only; nothing is copied here).
//!
//! The tests are env-gated: they need a game install, found through `$CS2_MCP_PROJECT_ROOT`
//! (default: this checkout). Without one they print `skipped: sample missing`
//! and pass, so `cargo test --workspace` stays green on a machine that has no CS2 install.
//!
//! * [`glass_probe_byte_round_trip`] — the `.vmdl` the whole ModelDoc work is built on;
//! * [`whole_content_tree_round_trips`] — every `<!-- kv3 … -->` file under
//!   `content/` of the install, minus the two documented exceptions (both contain comments
//!   and/or whitespace-only lines, which the parser skips; see the crate docs).
//!
//! These are the tests that pin the writer's layout: they are the reason `write` emits CRLF
//! with no final newline, `[  ]` for empty arrays, a trailing space after `=` before a block,
//! four numbers per line in a long numeric array, and Valve's column-0 `#[` blob layout.

use std::fs;
use std::path::{Path, PathBuf};

/// Valve-authored KV3 files that contain comments and/or whitespace-only lines: the parser skips
/// everything a KV3 reader ignores, so these two keep their content but not their exact layout.
const DOCUMENTED_NON_IDENTICAL: &[&str] = &["propdata.vdata", "soundevents_addon.vsndevts"];

/// KV3 files we know must round-trip, relative to `content/` (the brief's samples).
const MUST_ROUND_TRIP: &[&str] = &[
    "csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl",
    "csgo/workshop/content_examples/breakable_glass/breakable_glass_01.vmdl",
    "csgo/workshop/paintkits/templates/solid_color_case_hardening.vcompmat",
    "csgo/workshop/paintkits/templates/patina.vcompmat",
    "csgo/items/assets/stickers/workshop/example_paper.econitem",
    "csgo/items/assets/paintkits/workshop/aa_paintkit_example.econitem",
    "csgo_addons/addon_template/postprocess/xxx_mapname_xxx.vpost",
];

fn content_root() -> Option<PathBuf> {
    let project = std::env::var("CS2_MCP_PROJECT_ROOT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string());
    let content = Path::new(&project).join("workshop/run/app/content");
    content.is_dir().then_some(content)
}

/// Every file under `dir` whose first bytes are a KV3 header (checked with an 8-byte read
/// before anything else, so a large content tree stays cheap).
fn kv3_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.is_dir() {
            kv3_files(&path, out);
        } else if meta.len() < 32 * 1024 * 1024 && starts_with_kv3_header(&path) {
            out.push(path);
        }
    }
}

fn starts_with_kv3_header(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 8];
    match fs::File::open(path).and_then(|mut file| file.read_exact(&mut head)) {
        Ok(()) => &head == b"<!-- kv3",
        Err(_) => false,
    }
}

#[test]
fn glass_probe_byte_round_trip() {
    let Some(content) = content_root() else {
        println!("skipped: sample missing (no content tree)");
        return;
    };
    let sample = content.join("csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl");
    if !sample.is_file() {
        println!("skipped: sample missing ({})", sample.display());
        return;
    }
    let text = fs::read_to_string(&sample).expect("utf-8");
    let doc = cs2_kv3::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", sample.display()));
    assert_eq!(cs2_kv3::write(&doc), text, "{} did not round-trip", sample.display());
    assert!(text.contains("\r\n") && !text.ends_with('\n'));

    let root = doc.root.get("rootNode").and_then(|v| v.as_object()).expect("rootNode");
    assert_eq!(root.get("_class").and_then(|v| v.as_str()), Some("RootNode"));
    let children = root.get("children").and_then(|v| v.as_array()).expect("children");
    assert!(children.iter().any(|child| child
        .as_object()
        .and_then(|c| c.get("_class"))
        .and_then(|v| v.as_str())
        == Some("BreakPieceList")));
}

#[test]
fn listed_samples_round_trip() {
    let Some(content) = content_root() else {
        println!("skipped: sample missing (no content tree)");
        return;
    };
    let mut checked = 0;
    for rel in MUST_ROUND_TRIP {
        let path = content.join(rel);
        if !path.is_file() {
            println!("skipped: sample missing ({})", path.display());
            continue;
        }
        let text = fs::read_to_string(&path).expect("utf-8");
        let doc = cs2_kv3::parse(&text).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert_eq!(cs2_kv3::write(&doc), text, "{rel} did not round-trip byte for byte");
        checked += 1;
    }
    assert!(checked > 0, "no listed samples found; nothing was verified");
}

#[test]
fn whole_content_tree_round_trips() {
    let Some(content) = content_root() else {
        println!("skipped: sample missing (no content tree)");
        return;
    };
    let mut files = Vec::new();
    kv3_files(&content, &mut files);
    files.sort();
    assert!(files.len() > 50, "expected the shipped content tree, found {} files", files.len());

    let mut identical = 0;
    let mut differing: Vec<(String, PathBuf, String)> = Vec::new();
    let mut unreadable = 0;
    for path in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        match cs2_kv3::parse(&text) {
            Ok(doc) => {
                let written = cs2_kv3::write(&doc);
                if written == text {
                    identical += 1;
                } else {
                    differing.push((name, path.clone(), first_difference(&written, &text)));
                }
            }
            Err(error) => differing.push((name, path.clone(), format!("parse error: {error}"))),
        }
    }
    println!(
        "kv3 files: {} (identical {identical}, differing {}, unreadable {unreadable})",
        files.len(),
        differing.len()
    );
    let mut unexpected = Vec::new();
    for (name, path, why) in &differing {
        if user_addon_layout_difference(&content, path, why) {
            // Bytes may differ, content may not: the values must survive a write and a
            // re-parse, and writing the result again must change nothing (idempotent writer).
            let text = fs::read_to_string(path).expect("utf-8 (it parsed above)");
            let doc = cs2_kv3::parse(&text).expect("it parsed above");
            let written = cs2_kv3::write(&doc);
            let again = cs2_kv3::parse(&written)
                .unwrap_or_else(|e| panic!("{name}: the written document does not parse: {e}"));
            assert!(again == doc, "{name}: values changed across write and re-parse");
            assert!(cs2_kv3::write(&again) == written, "{name}: the writer is not idempotent");
            println!("  user addon, byte layout differs (values round-trip): {name}: {why}");
            continue;
        }
        // the MCP tools back a file up as `<name>.bak-<unix secs>` before rewriting it: a backup
        // of a documented file is the same Valve-authored layout
        let original = match name.rsplit_once(".bak-") {
            Some((stem, secs)) if !secs.is_empty() && secs.bytes().all(|b| b.is_ascii_digit()) => stem,
            _ => name.as_str(),
        };
        if DOCUMENTED_NON_IDENTICAL.contains(&original) {
            println!("  expected difference: {name}: {why}");
        } else {
            unexpected.push(format!("{name}: {why}"));
        }
    }
    assert!(
        unexpected.is_empty(),
        "{} file(s) that should round-trip did not:\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );
    assert!(
        identical + differing.len() + unreadable == files.len(),
        "accounting mismatch"
    );
}

/// Files under a user addon (`content/csgo_addons/<addon>/`, any addon but Valve's
/// `addon_template`) are written by whoever builds a map there, and some by hand or by a script
/// (e.g. `m_ColorMin = [ 60, 230, 70, 255 ]` on one line in a `.vpcf`), so they are not in
/// Valve's writer layout and the install grows them without notice. For those, a byte
/// difference is reported and not asserted (the caller asserts the values round-trip and the
/// writer is idempotent); a parse error still fails the test. Everything
/// outside user addons (the shipped `csgo/` tree, `addon_template`) must come back byte for byte.
fn user_addon_layout_difference(content: &Path, path: &Path, why: &str) -> bool {
    if why.starts_with("parse error") {
        return false;
    }
    let Ok(rel) = path.strip_prefix(content) else { return false };
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    parts.next().is_some_and(|c| c == "csgo_addons")
        && parts.next().is_some_and(|addon| addon != "addon_template")
}

fn first_difference(written: &str, original: &str) -> String {
    let at = written
        .bytes()
        .zip(original.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or(written.len().min(original.len()));
    let window = |text: &str| {
        let bytes = text.as_bytes();
        let start = at.saturating_sub(20).min(bytes.len());
        let end = (at + 20).min(bytes.len());
        String::from_utf8_lossy(&bytes[start..end]).escape_debug().to_string()
    };
    format!(
        "byte {at} (written {} vs file {}, lengths {}/{})",
        window(written),
        window(original),
        written.len(),
        original.len()
    )
}

/// The ValveResourceFormat (MIT) KV3 test inputs, which exist only next to a checkout of it.
///
/// They are hand-written, not Valve output: LF, space indentation, comments, `"""` strings and
/// unusual escapes (`\v`, `\?`, `\'`), and VRF's own writer normalises an unknown escape by
/// dropping its backslash. So the assertion here is *parse and stay stable*, not byte identity.
#[test]
fn valve_resource_format_escaping_samples() {
    let project = std::env::var("CS2_MCP_PROJECT_ROOT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string());
    let dir = Path::new(&project).join("ValveResourceFormat/Tests/Files/KeyValues");
    if !dir.is_dir() {
        println!("skipped: sample missing ({})", dir.display());
        return;
    }
    let mut samples: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("readable")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|e| e == "kv3").unwrap_or(false))
        .collect();
    samples.sort();
    assert!(!samples.is_empty(), "no .kv3 samples in {}", dir.display());

    for path in &samples {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let text = fs::read_to_string(path).expect("utf-8");
        let doc = cs2_kv3::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let written = cs2_kv3::write(&doc);
        let again = cs2_kv3::parse(&written).unwrap_or_else(|e| panic!("{name} rewritten: {e}"));
        assert_eq!(again, doc, "{name}: rewrite changed the tree");
        println!(
            "{name}: parsed ({} entries), byte-identical: {}",
            doc.root.len(),
            written == text
        );
    }
}
