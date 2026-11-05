//! Round trips: every canonical fixture must come back out byte for byte, and re-parsing written
//! text must give the same tree.
//!
//! The fixtures in `fixtures/` are in Valve's own layout (CRLF, no final newline), generated from
//! the same rules that reproduce 93 of the 95 shipped KV3 files of a real game install — see
//! `tests/real_files.rs`. Fixtures whose name starts with `parse_only_` are laid out by hand to
//! exercise the parser (comments, `"""` strings, loose spacing) and are checked structurally.

use std::fs;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixtures())
        .expect("fixtures dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".kv3"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no fixtures found");
    names
}

#[test]
fn every_fixture_round_trips_byte_for_byte() {
    let mut checked = 0;
    for name in fixture_names() {
        if name.starts_with("parse_only_") {
            continue;
        }
        let path = fixtures().join(&name);
        let text = fs::read_to_string(&path).expect("fixture is utf-8");
        let doc = cs2_kv3::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let written = cs2_kv3::write(&doc);
        assert_eq!(written, text, "{name} does not round-trip byte for byte");
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} fixtures round-tripped");
}

#[test]
fn rewrite_is_stable_and_structurally_equal() {
    for name in fixture_names() {
        let text = fs::read_to_string(fixtures().join(&name)).expect("fixture is utf-8");
        let first = cs2_kv3::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let once = cs2_kv3::write(&first);
        let second = cs2_kv3::parse(&once).unwrap_or_else(|e| panic!("{name} rewritten: {e}"));
        assert_eq!(first, second, "{name}: parse(write(parse(x))) changed the tree");
        assert_eq!(cs2_kv3::write(&second), once, "{name}: write is not idempotent");
    }
}

#[test]
fn canonical_fixtures_use_valve_line_endings() {
    for name in fixture_names() {
        if name.starts_with("parse_only_") {
            continue;
        }
        let text = fs::read_to_string(fixtures().join(&name)).expect("fixture is utf-8");
        assert!(text.contains("\r\n"), "{name} should be CRLF");
        assert!(!text.replace("\r\n", "").contains('\n'), "{name} mixes line endings");
        assert!(!text.ends_with('\n'), "{name} should not end with a newline");
    }
}

#[test]
fn lf_style_writes_lf_and_a_final_newline() {
    let text = fs::read_to_string(fixtures().join("vmdl_sample.kv3")).expect("fixture");
    let doc = cs2_kv3::parse(&text).expect("parses");
    let lf = cs2_kv3::write_styled(
        &doc,
        cs2_kv3::Style { newline: cs2_kv3::Newline::Lf, final_newline: true },
    );
    assert!(!lf.contains('\r'));
    assert!(lf.ends_with("}\n"));
    assert_eq!(
        cs2_kv3::write_styled(&cs2_kv3::parse(&lf).expect("parses"), cs2_kv3::Style::VALVE),
        text
    );
}

#[test]
fn writes_are_stable_after_editing() {
    // A value changed in the tree comes back out in Valve's layout (the `model_edit` path).
    let text = fs::read_to_string(fixtures().join("vmdl_sample.kv3")).expect("fixture");
    let mut doc = cs2_kv3::parse(&text).expect("parses");
    let root = doc
        .root
        .get_mut("rootNode")
        .and_then(|v| v.as_object_mut())
        .expect("rootNode");
    root.insert("document_sub_type".to_string(), cs2_kv3::Value::String("X".into()));
    let written = cs2_kv3::write(&doc);
    assert!(written.contains("\tdocument_sub_type = \"X\"\r\n"));
    assert_eq!(cs2_kv3::write(&cs2_kv3::parse(&written).expect("reparses")), written);
    assert_eq!(written.matches("document_sub_type").count(), 1);
}
