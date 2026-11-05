//! Byte-identical round trips on the **real** sample files (read in place; nothing is copied here).
//!
//! The tests are env-gated exactly like `crates/cs2-kv3/tests/real_files.rs`: they need the game install
//! and the upstream sources, found through `$CS2_MCP_PROJECT_ROOT` (default: this checkout). Without them they print `skipped: … missing` and pass, so
//! `cargo test --workspace` stays green on a machine that has neither.
//!
//! * [`content_vmaps_round_trip_byte_for_byte`] — every `*.vmap` under `content/` of the workshop
//!   install (Hammer writes `binary 9`);
//! * [`upstream_resources_round_trip_byte_for_byte`] — every `.dmx`/`.vmap` under
//!   `target/upstream-src/Datamodel.NET/Tests/Resources` that is binary encoded (the keyvalues2
//!   ones belong to the keyvalues2 codec), which is where the binary 4 and binary 5 samples are;
//! * [`editing_one_string_appends_to_the_table`] — one string attribute of one real map changed to
//!   a new value, which is the edit our consumer (hammer-mcp) performs.
//!
//! The byte-for-byte comparison is **asserted**, not reported: [`cs2_dmx::save`] writes Valve's own
//! layout (see `codec::binary`'s module docs for how it was derived), so `encode(decode(file))`
//! has to be `file`. The offset a file first differs at is printed before the assertion fails.
//!
//! Run with `-- --nocapture` to see the per-file lines and the summaries.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use cs2_dmx::codec;
use cs2_dmx::{Attribute, Datamodel, Element, ElementId};

/// The project checkout: the gate for everything below.
fn project_root() -> PathBuf {
    PathBuf::from(std::env::var("CS2_MCP_PROJECT_ROOT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string()))
}

/// This repo's checkout under a project root: `<root>/cs2-mcp` in the older parent layout, else
/// the root itself (`cs2_mcp_common::mcp_repo_dir`'s rule; this crate does not depend on it).
fn repo_of(project: &Path) -> PathBuf {
    let nested = project.join("cs2-mcp");
    if nested.join("Cargo.toml").is_file() {
        nested
    } else {
        project.to_path_buf()
    }
}

/// Every `*.vmap` under the workshop install's content directory, in a stable order.
fn content_vmaps() -> Option<Vec<PathBuf>> {
    let content = project_root().join("workshop/run/app/content");
    if !content.is_dir() {
        println!("skipped: {} missing", content.display());
        return None;
    }
    let mut files = Vec::new();
    collect(&content, &mut files);
    files.retain(|path| path.extension().is_some_and(|extension| extension == "vmap"));
    files.sort();
    Some(files)
}

/// The upstream test resources: the `.dmx` and `.vmap` fixtures next to the C# tests.
fn upstream_resources() -> Option<Vec<PathBuf>> {
    let resources = repo_of(&project_root()).join("target/upstream-src/Datamodel.NET/Tests/Resources");
    if !resources.is_dir() {
        println!("skipped: {} missing", resources.display());
        return None;
    }
    let mut files = Vec::new();
    collect(&resources, &mut files);
    files.retain(|path| path.extension().is_some_and(|extension| extension == "dmx" || extension == "vmap"));
    files.sort();
    Some(files)
}

/// Every file below `dir`, depth first.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The header line, up to and including its `>`: a binary file continues with `\n\0`, a text one
/// with a newline (the crate's own header parser stops at the same `>`).
fn header_line(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().take(129).position(|byte| *byte == b'>')?;
    Some(String::from_utf8_lossy(&bytes[..=end]).into_owned())
}

/// A short name for the log lines, relative to the project checkout when it is below it.
fn name_of(path: &Path) -> String {
    path.strip_prefix(project_root())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// The number of leading bytes two files share.
fn common_prefix(left: &[u8], right: &[u8]) -> usize {
    left.iter().zip(right).take_while(|(left, right)| left == right).count()
}

/// The length of a string table in bytes: the count field, then one NUL-terminated string per
/// entry.
fn table_len(table: &[String], count_width: usize) -> usize {
    count_width + table.iter().map(|value| value.len() + 1).sum::<usize>()
}

/// What one file did.
struct Outcome {
    name: String,
    header: String,
    version: i32,
    elements: usize,
    stubs: usize,
    prefix: usize,
    table: usize,
    identical: bool,
}

/// Decode, re-encode and re-decode one file, asserting everything including byte identity.
///
/// Returns the outcome, and (when `keep`) the decoded model and the re-encoded bytes, which the
/// edit test needs.
fn check(path: &Path, keep: bool) -> (Outcome, Option<(Datamodel, Vec<u8>)>) {
    let name = name_of(path);
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("{name}: {error}"));
    let head = header_line(&bytes).unwrap_or_else(|| panic!("{name}: no header"));

    let dm = codec::load(&bytes).unwrap_or_else(|error| panic!("{name}: decode: {error:?}"));
    let re_encoded = codec::save(&dm).unwrap_or_else(|error| panic!("{name}: encode: {error:?}"));
    let reloaded = codec::load(&re_encoded).unwrap_or_else(|error| panic!("{name}: decode of the re-encoded bytes: {error:?}"));
    let again = codec::save(&reloaded).unwrap_or_else(|error| panic!("{name}: second encode: {error:?}"));

    assert_eq!(again, re_encoded, "{name}: encoding is not stable across a load");
    assert_eq!(dm.prefix_attributes, reloaded.prefix_attributes, "{name}: prefix attributes");
    assert_same_elements(&dm, &reloaded, &name);
    if let Some(root) = dm.root {
        assert_eq!(Some(dm.elements[root.0].id), reloaded.root.map(|root| reloaded.elements[root.0].id), "{name}: root");
    }

    let outcome = Outcome {
        name,
        header: head,
        version: dm.encoding_version,
        elements: dm.elements.len(),
        stubs: dm.elements.iter().filter(|element| element.stub).count(),
        prefix: dm.prefix_attributes.len(),
        table: dm.binary_string_table.len(),
        identical: re_encoded == bytes,
    };
    if !outcome.identical {
        let offset = common_prefix(&re_encoded, &bytes);
        let window = |bytes: &[u8]| {
            let end = (offset + 16).min(bytes.len());
            bytes[offset.min(bytes.len())..end].iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(" ")
        };
        println!(
            "    {}: {} | {} elements ({} stubs), {} prefix attrs, table {}, {} bytes -> {} bytes",
            outcome.name,
            outcome.header,
            outcome.elements,
            outcome.stubs,
            outcome.prefix,
            outcome.table,
            bytes.len(),
            re_encoded.len()
        );
        println!(
            "      differs at {offset} of {} ({} bytes shared): file {} | ours {}",
            bytes.len(),
            offset,
            window(&bytes),
            window(&re_encoded)
        );
    }

    let kept = if keep { Some((dm, re_encoded)) } else { None };
    (outcome, kept)
}

/// Every element survives a load with its values, and no element appears or disappears.
fn assert_same_elements(dm: &Datamodel, reloaded: &Datamodel, name: &str) {
    assert_eq!(dm.elements.len(), reloaded.elements.len(), "{name}: element count");
    for (index, element) in dm.elements.iter().enumerate() {
        let other = &reloaded.elements[index];
        assert!(
            same_element(element, other, dm, reloaded),
            "{name}: element {index} ({}/{}) survived differently",
            element.class_name,
            element.name
        );
    }
}

/// Every `*.vmap` Hammer wrote under the workshop install's content directory.
#[test]
fn content_vmaps_round_trip_byte_for_byte() {
    let Some(files) = content_vmaps() else { return };
    assert!(!files.is_empty(), "the content tree holds no .vmap at all");

    let outcomes: Vec<Outcome> = files.iter().map(|path| check(path, false).0).collect();
    let identical = outcomes.iter().filter(|outcome| outcome.identical).count();
    println!("content: {} .vmap files, {identical} re-encoded byte for byte", outcomes.len());
    for outcome in &outcomes {
        println!(
            "  {:<12} {:>7} elements {:>4} stubs {:>2} prefix attrs {:>6} strings  identical {}",
            outcome.header.trim_start_matches("<!-- dmx encoding ").split(" format").next().unwrap_or("?"),
            outcome.elements,
            outcome.stubs,
            outcome.prefix,
            outcome.table,
            outcome.identical
        );
    }

    // The install grows as maps are made in it; the report was written against 29.
    assert!(
        outcomes.len() >= 29,
        "the workshop install the report was written against holds 29 maps; found {}",
        outcomes.len()
    );
    for outcome in &outcomes {
        assert!(outcome.identical, "{}: {} did not come back byte for byte", outcome.name, outcome.header);
    }
}

/// Every binary fixture of the upstream C# test suite: `binary4.dmx` is a binary 4 file,
/// `taunt05_b5.dmx` a binary 5 one and the rest are binary 9.
#[test]
fn upstream_resources_round_trip_byte_for_byte() {
    let Some(files) = upstream_resources() else { return };
    assert!(!files.is_empty(), "the resources directory holds nothing to read");

    let mut outcomes = Vec::new();
    for path in &files {
        let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let Some(head) = header_line(&bytes) else {
            panic!("{}: no header", path.display());
        };
        if !head.contains("encoding binary") {
            println!("  {head}: not a binary file (the keyvalues2 codec covers it)");
            continue;
        }

        let outcome = check(path, false).0;
        println!(
            "  {:<44} binary {:>2} {:>7} elements {:>4} stubs {:>2} prefix attrs {:>6} strings  identical {}",
            outcome.name, outcome.version, outcome.elements, outcome.stubs, outcome.prefix, outcome.table, outcome.identical
        );
        outcomes.push(outcome);
    }

    assert!(outcomes.len() >= 9, "expected the binary fixtures, checked {}", outcomes.len());
    let versions: BTreeSet<i32> = outcomes.iter().map(|outcome| outcome.version).collect();
    assert!(versions.contains(&4) && versions.contains(&5) && versions.contains(&9), "the fixtures cover 4, 5 and 9: {versions:?}");
    for outcome in &outcomes {
        assert!(outcome.identical, "{}: binary {} did not come back byte for byte", outcome.name, outcome.version);
    }
}

/// One string attribute of one real map changed to a value no file holds: the new string is
/// appended to the table, every other string keeps its index, and the only bytes that move are the
/// table's own length and the changed attribute's value.
#[test]
fn editing_one_string_appends_to_the_table() {
    let Some(files) = content_vmaps() else { return };

    // The first file with a plain string attribute and the first element of it that has one, so
    // the test does not depend on one map.
    let mut picked = None;
    for path in &files {
        let Ok(original) = fs::read(path) else { continue };
        let Ok(dm) = codec::load(&original) else { continue };
        if let Some((element, attribute)) = pick(&dm) {
            picked = Some((path, dm, original, element, attribute));
            break;
        }
    }
    let (path, mut dm, original, element, attribute) = picked.expect("no map in the content tree has a string attribute");

    let attribute_name = dm.elements[element].attributes[attribute].0.clone();
    let old_value = match &dm.elements[element].attributes[attribute].1 {
        Attribute::String(value) => value.clone(),
        other => panic!("not a string: {other:?}"),
    };
    let new_value = format!("opencode_wp3_edited_{}", dm.elements[element].id);
    dm.elements[element].attributes[attribute].1 = Attribute::String(new_value.clone());
    let bytes = codec::save(&dm).expect("save the edited map");

    // The first byte that moves is the table's own length: everything before it, the header and
    // the prefix block, is untouched, and so is every string the table already had.
    let count_width = if dm.encoding_version < 4 { 2 } else { 4 };
    let old_count = dm.binary_string_table.len() as i32;
    let table = &dm.binary_string_table;
    let table_at = common_prefix(&bytes, &original);
    assert_eq!(
        bytes[..table_at],
        original[..table_at],
        "the header and the prefix block are untouched"
    );
    assert_eq!(i32::from_le_bytes(original[table_at..table_at + 4].try_into().unwrap()), old_count, "the file's table length");
    assert_eq!(i32::from_le_bytes(bytes[table_at..table_at + 4].try_into().unwrap()), old_count + 1, "one more string");
    let old_table_end = table_at + table_len(table, count_width);
    assert_eq!(
        bytes[table_at + count_width..old_table_end],
        original[table_at + count_width..old_table_end],
        "every string the table already had kept its position"
    );
    assert_eq!(
        &bytes[old_table_end..old_table_end + new_value.len() + 1],
        [new_value.as_bytes(), b"\0"].concat(),
        "the new string is appended"
    );
    // The appended string sits between the old table and the bodies, so everything after it is the
    // original again, except the one value that changed: its index was the value's position in the
    // table and is now the position of the appended string.
    let body_at = old_table_end + new_value.len() + 1;
    let moved: Vec<usize> = bytes[body_at..]
        .iter()
        .zip(&original[old_table_end..])
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(offset, _)| offset)
        .collect();
    assert!(!moved.is_empty(), "the changed value has to differ");
    let offset = moved[0];
    assert!(
        moved.iter().all(|other| *other < offset + 4),
        "one int32 moved, but {} bytes did: {:?}…",
        moved.len(),
        &moved[..moved.len().min(8)]
    );
    let old_index = table.iter().position(|value| *value == old_value).expect("the old value is in the table") as i32;
    let (original_at, new_at) = (old_table_end + offset, body_at + offset);
    assert_eq!(i32::from_le_bytes(original[original_at..original_at + 4].try_into().unwrap()), old_index);
    assert_eq!(i32::from_le_bytes(bytes[new_at..new_at + 4].try_into().unwrap()), old_count);

    // The edit round-trips: the same model, with exactly the one attribute different.
    let reloaded = codec::load(&bytes).expect("decode the edited map");
    assert_same_elements(&dm, &reloaded, "the edit");
    assert_eq!(reloaded.binary_string_table.len(), old_count as usize + 1);
    assert_eq!(reloaded.binary_string_table.last().map(String::as_str), Some(new_value.as_str()));
    assert_eq!(reloaded.elements[element].get(&attribute_name), Some(&Attribute::String(new_value.clone())));

    // ... and the model it came from still has the old value (nothing was mutated in place).
    let original_model = codec::load(&original).expect("decode the original map");
    assert_eq!(original_model.elements[element].get(&attribute_name), Some(&Attribute::String(old_value.clone())));

    println!(
        "edited {} element #{element} {:?} {:?} -> {:?}: {} -> {} bytes, {table_at} bytes identical before the table, \
         new string at index {old_count} (the old value was index {old_index})",
        name_of(path),
        attribute_name,
        old_value,
        new_value,
        original.len(),
        bytes.len()
    );
}

/// The element and attribute the edit test picked: the first plain string attribute of an element
/// that is not the prefix element, preferring one with a value (an empty string is a corner case).
/// The prefix element is left out because its attributes are also in the prefix block, which the
/// model keeps as its own copy: editing one would leave the other behind (the frozen model has no
/// link between them).
fn pick(dm: &Datamodel) -> Option<(usize, usize)> {
    let prefix_element = (dm.encoding_version >= 9 && !dm.prefix_attributes.is_empty()).then_some(1);
    let string_attribute = |element: &Element| element.attributes.iter().position(|(_, a)| matches!(a, Attribute::String(_)));
    let candidates = || {
        dm.elements
            .iter()
            .enumerate()
            .filter(|(index, element)| Some(*index) != prefix_element && !element.stub)
    };
    candidates()
        .find_map(|(index, element)| {
            let attribute = element.attributes.iter().position(|(_, a)| matches!(a, Attribute::String(value) if !value.is_empty()))?;
            Some((index, attribute))
        })
        .or_else(|| candidates().find_map(|(index, element)| Some((index, string_attribute(element)?))))
}

/// Two elements are the same when their values are, comparing element references by GUID.
fn same_element(left: &Element, right: &Element, first: &Datamodel, second: &Datamodel) -> bool {
    left.class_name == right.class_name
        && left.name == right.name
        && left.id == right.id
        && left.stub == right.stub
        && left.attributes.len() == right.attributes.len()
        && left
            .attributes
            .iter()
            .zip(right.attributes.iter())
            .all(|((left_name, left_value), (right_name, right_value))| {
                left_name == right_name && same_attribute(left_value, right_value, first, second)
            })
}

/// Two attribute values are the same when their values are, comparing element references by GUID.
fn same_attribute(left: &Attribute, right: &Attribute, first: &Datamodel, second: &Datamodel) -> bool {
    let guid = |dm: &Datamodel, id: &Option<ElementId>| id.map(|id| dm.elements[id.0].id);
    match (left, right) {
        (Attribute::Element(left), Attribute::Element(right)) => guid(first, left) == guid(second, right),
        (Attribute::ElementArray(left), Attribute::ElementArray(right)) => {
            left.len() == right.len()
                && left.iter().zip(right.iter()).all(|(left, right)| guid(first, left) == guid(second, right))
        }
        _ => left == right,
    }
}
