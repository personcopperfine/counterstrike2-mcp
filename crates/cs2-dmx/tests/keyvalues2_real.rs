//! The **real** keyvalues2 sample files, read in place and never copied.
//!
//! Both come from `target/upstream-src/Datamodel.NET/Tests/Resources` (they are Valve game
//! content — Team Fortress 2 and Counter-Strike 2 — so they are not committed here) and live
//! next to their binary twins `cs2_map.vmap` / `reflectiontest.vmap`; `cross_codec.rs` compares the
//! two decodes by GUID. These tests state structural facts read off the text by hand.
//!
//! The tests are env-gated exactly like `crates/cs2-kv3/tests/real_files.rs`: without the files they
//! print `skipped: … missing` and pass, so `cargo test --workspace` stays green on a checkout
//! that has no fixtures. `$CS2_MCP_PROJECT_ROOT` selects the checkout (default: this one); the
//! files are looked up under `target/upstream-src/Datamodel.NET/Tests/Resources`.
//!
//! Both files are **`dmxconvert`/Hammer text**, not upstream's writer: their layout is what
//! [`keyvalues2::Layout::Valve`] reproduces, so the byte-for-byte round trips here are the
//! tests that pin that layout against real content.

use std::fs;
use std::path::{Path, PathBuf};

use cs2_dmx::codec::{keyvalues2, parse_header};
use cs2_dmx::{Attribute, Datamodel, Error};

/// The two text files and the facts read off them by hand (element count, root class, prefix
/// block), which also describe their binary twins, and the format version of the header.
struct Sample {
    name: &'static str,
    format_version: i32,
    elements: usize,
    top_level_blocks: usize,
    root_class: &'static str,
    root_guid: &'static str,
    prefix_guid: &'static str,
    prefix_attributes: usize,
}

const SAMPLES: [Sample; 2] = [
    // 43 `"id" "elementid"` lines, 5 blocks at column 0, the first of them CMapRootElement
    Sample {
        name: "cs2_map.vmap.txt",
        format_version: 35,
        elements: 43,
        top_level_blocks: 5,
        root_class: "CMapRootElement",
        root_guid: "c73e29a3-b55d-40db-afdc-61203fabce54",
        prefix_guid: "baec7cd3-e2e3-4c81-890c-b7271d646563",
        prefix_attributes: 1,
    },
    // 72 ids, 2 blocks, one 18598-byte thumbnail in the prefix (wrapped every 40 bytes)
    Sample {
        name: "reflectiontest.vmap.txt",
        format_version: 40,
        elements: 72,
        top_level_blocks: 2,
        root_class: "CMapRootElement",
        root_guid: "391aab3f-ca0a-4064-9db9-375eafa06d65",
        prefix_guid: "d5547f30-69f9-4f12-96c1-f45e95deb8fc",
        prefix_attributes: 3,
    },
];

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

fn resources() -> Option<PathBuf> {
    let project = std::env::var("CS2_MCP_PROJECT_ROOT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string());
    let dir = repo_of(Path::new(&project)).join("target/upstream-src/Datamodel.NET/Tests/Resources");
    dir.is_dir().then_some(dir)
}

/// Read one sample; `None` (with a loud line) when it is not there.
fn read_sample(name: &str) -> Option<Vec<u8>> {
    let Some(dir) = resources() else {
        println!(
            "skipped: keyvalues2 samples missing (set $CS2_MCP_PROJECT_ROOT to the checkout \
             that holds target/upstream-src)"
        );
        return None;
    };
    let path = dir.join(name);
    match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            println!("skipped: {} missing ({err})", path.display());
            None
        }
    }
}

fn decode(bytes: &[u8]) -> Result<(cs2_dmx::Header, Datamodel), Error> {
    let (header, offset) = parse_header(bytes)?;
    let dm = keyvalues2::decode(&header, &bytes[offset..])?;
    Ok((header, dm))
}

/// The first byte where two texts differ, with the line and column and the surrounding text,
/// so a layout failure says what the writer did differently. Writes the mismatching output to
/// `$CARGO_TARGET_TMPDIR/reencoded/<name>` for a line-by-line look.
fn assert_identical(name: &str, expected: &[u8], actual: &[u8]) {
    if expected == actual {
        return;
    }
    let offset = expected
        .iter()
        .zip(actual.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(expected.len().min(actual.len()));
    let window = |bytes: &[u8]| {
        let start = offset.saturating_sub(60);
        String::from_utf8_lossy(&bytes[start..(offset + 60).min(bytes.len())]).into_owned()
    };
    let line = expected[..offset].iter().filter(|&&b| b == b'\n').count() + 1;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("reencoded");
    fs::create_dir_all(&dir).expect("diagnosis dir");
    fs::write(dir.join(name), actual).expect("write the re-encoded file");
    panic!(
        "{name}: re-encoding is not byte-identical, first difference at byte {offset} (line \
         {line}); the output is in {}\n  expected: {}\n  actual:   {}",
        dir.join(name).display(),
        window(expected),
        window(actual),
    );
}

/// Decode -> encode gives the file back byte for byte, and decoding that output again is the
/// same model. This is what pins Hammer's text layout.
fn round_trip(entry: &Sample) {
    let Some(bytes) = read_sample(entry.name) else { return };
    let (_, dm) = decode(&bytes).unwrap_or_else(|err| panic!("{}: decode failed: {err}", entry.name));
    let encoded = keyvalues2::encode(&dm)
        .unwrap_or_else(|err| panic!("{}: encode failed: {err}", entry.name));
    assert_identical(entry.name, &bytes, &encoded);
    let (_, again) = decode(&encoded).unwrap();
    assert_same_model(&dm, &again, entry.name);
}

#[test]
fn every_sample_round_trips_byte_for_byte() {
    for entry in &SAMPLES {
        round_trip(entry);
    }
}

/// Upstream's layout is a *model* round trip on the same files: the bytes differ (its trailing
/// blocks come out in reference order, and a long binary on one line), but nothing of the model
/// is lost, and nothing of it moves either.
#[test]
fn upstream_layout_keeps_the_model() {
    for entry in &SAMPLES {
        let Some(bytes) = read_sample(entry.name) else { continue };
        let (_, dm) = decode(&bytes).unwrap();
        let encoded = keyvalues2::encode_with(&dm, keyvalues2::Layout::Upstream).unwrap();
        let (_, again) = decode(&encoded).unwrap();
        assert_same_model(&dm, &again, entry.name);
        assert_eq!(keyvalues2::encode_with(&again, keyvalues2::Layout::Upstream).unwrap(), encoded);
    }
}

/// Two models hold the same thing when they have the same prefix attributes, the same root and
/// an element per GUID with the same class, name and attributes in order — element references
/// compare through each side's own arena, since the two walk orders differ (upstream's
/// `AssertEquivalent`).
fn assert_same_model(expected: &Datamodel, actual: &Datamodel, context: &str) {
    assert_eq!(expected.elements.len(), actual.elements.len(), "{context}: element count");
    assert_eq!(expected.prefix_attributes, actual.prefix_attributes, "{context}: prefix");
    let ids = |dm: &Datamodel| -> Vec<cs2_dmx::Guid> { dm.elements.iter().map(|e| e.id).collect() };
    let (expected_ids, actual_ids) = (ids(expected), ids(actual));
    assert_eq!(
        expected.root.map(|id| expected_ids[id.0]),
        actual.root.map(|id| actual_ids[id.0]),
        "{context}: root"
    );
    let by_guid: std::collections::HashMap<cs2_dmx::Guid, &cs2_dmx::Element> =
        actual.elements.iter().map(|element| (element.id, element)).collect();
    let refs = |items: &[Option<cs2_dmx::ElementId>], ids: &[cs2_dmx::Guid]| -> Vec<Option<cs2_dmx::Guid>> {
        items.iter().map(|item| item.map(|id| ids[id.0])).collect()
    };
    for element in &expected.elements {
        let Some(other) = by_guid.get(&element.id) else {
            panic!("{context}: {} is missing from the re-encoded model", text_of(&element.id));
        };
        let at = text_of(&element.id);
        assert_eq!(element.class_name, other.class_name, "{context}: class of {at}");
        assert_eq!(element.name, other.name, "{context}: name of {at}");
        assert_eq!(element.attributes.len(), other.attributes.len(), "{context}: attributes of {at}");
        for ((name, value), (other_name, other_value)) in
            element.attributes.iter().zip(other.attributes.iter())
        {
            assert_eq!(name, other_name, "{context}: attribute name of {at}");
            match (value, other_value) {
                (Attribute::Element(a), Attribute::Element(b)) => assert_eq!(
                    a.map(|id| expected_ids[id.0]),
                    b.map(|id| actual_ids[id.0]),
                    "{context}: {at}.{name}"
                ),
                (Attribute::ElementArray(a), Attribute::ElementArray(b)) => assert_eq!(
                    refs(a, &expected_ids),
                    refs(b, &actual_ids),
                    "{context}: {at}.{name}"
                ),
                (a, b) => assert_eq!(a, b, "{context}: {at}.{name}"),
            }
        }
    }
}

/// The structural facts about each file, read off the text by hand — the same map as its
/// `.vmap` twin: how many elements the file holds, which one is the root, and what the prefix
/// block carries. This is the keyvalues2 half of the cross-encoding check.
#[test]
fn decoded_structure_matches_the_files() {
    for entry in &SAMPLES {
        let Some(bytes) = read_sample(entry.name) else { continue };
        let (header, dm) = decode(&bytes).unwrap();
        assert_eq!(header.encoding, "keyvalues2", "{}", entry.name);
        assert_eq!(header.encoding_version, 4, "{}", entry.name);
        assert_eq!(header.format, "vmap", "{}", entry.name);
        assert_eq!(header.format_version, entry.format_version, "{}", entry.name);

        assert_eq!(dm.elements.len(), entry.elements, "{}: element count", entry.name);
        let root = dm.root.expect("both maps have a root");
        let root_element = &dm.elements[root.0];
        assert_eq!(root_element.class_name, entry.root_class, "{}: root class", entry.name);
        assert_eq!(text_of(&root_element.id), entry.root_guid, "{}: root GUID", entry.name);

        let prefix = dm
            .elements
            .iter()
            .find(|element| element.class_name == keyvalues2::PREFIX_ELEMENT_CLASS)
            .expect("both files carry a $prefix_element$ block");
        assert_eq!(text_of(&prefix.id), entry.prefix_guid, "{}: prefix GUID", entry.name);
        // the prefix element mirrors the prefix attributes and is not the root
        assert_eq!(prefix.attributes, dm.prefix_attributes, "{}: prefix mirror", entry.name);
        assert_eq!(
            dm.prefix_attributes.len(),
            entry.prefix_attributes,
            "{}: prefix attribute count",
            entry.name
        );
        assert_ne!(Some(element_id_of(&dm, &prefix.id)), dm.root, "the prefix is not the root");

        // top-level blocks: the root plus the blocks after it, which are the multiply
        // referenced elements (counted by encoding and counting `}` at column 0)
        let text = String::from_utf8(bytes).unwrap();
        let blocks = text
            .lines()
            .filter(|line| line.starts_with('"') && !line.starts_with("\"$prefix_element$\""))
            .count();
        assert_eq!(blocks, entry.top_level_blocks, "{}: top-level blocks", entry.name);
    }
}

/// The prefix attributes themselves, spelled out: the map asset references both files carry,
/// and reflectiontest's JPEG thumbnail (18598 bytes in 40-byte lines) with its format and
/// FNV-1a hash, so the wrapped binary decoding is pinned against real content and not only
/// against the round trip.
#[test]
fn prefix_attribute_values_are_the_ones_the_files_hold() {
    let Some(bytes) = read_sample("cs2_map.vmap.txt") else { return };
    let (_, dm) = decode(&bytes).unwrap();
    assert_eq!(
        dm.prefix_attributes,
        vec![(
            "map_asset_references".to_string(),
            Attribute::StringArray(vec![
                "models/de_warden/rocks/large_rock_01.vmdl".to_string(),
                "materials/de_warden/rocks/rock_01/rock_01_dirt_02_blend.vmat".to_string(),
            ]),
        )]
    );

    let Some(bytes) = read_sample("reflectiontest.vmap.txt") else { return };
    let (_, dm) = decode(&bytes).unwrap();
    let names: Vec<&str> = dm.prefix_attributes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        ["asset_preview_thumbnail", "asset_preview_thumbnail_format", "map_asset_references"]
    );
    let Some((_, Attribute::Binary(thumbnail))) = dm
        .prefix_attributes
        .iter()
        .find(|(name, _)| name == "asset_preview_thumbnail")
    else {
        panic!("the thumbnail must decode as a binary value");
    };
    assert_eq!(thumbnail.len(), 18598);
    assert_eq!(&thumbnail[..8], &[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46]);
    // the JPEG end-of-image marker closes the blob, so no hex digit was lost in the wrapping
    assert_eq!(&thumbnail[thumbnail.len() - 8..], &[0xa0, 0x02, 0x8a, 0x28, 0xa0, 0x0f, 0xff, 0xd9]);
    assert_eq!(fnv1a(thumbnail), 0xd233_f8b3_b915_442a);
    assert_eq!(
        dm.prefix_attributes.iter().find(|(name, _)| name == "asset_preview_thumbnail_format"),
        Some(&("asset_preview_thumbnail_format".to_string(), Attribute::String("jpg".to_string())))
    );
    assert_eq!(
        dm.prefix_attributes.iter().find(|(name, _)| name == "map_asset_references"),
        Some(&(
            "map_asset_references".to_string(),
            Attribute::StringArray(vec![
                "materials/dev/reflectivity_30.vmat".to_string(),
                "maps/prefabs/asdf.vmap".to_string(),
            ])
        ))
    );
}

/// The same GUID text the file prints, from the arena. (The codec's own GUID text is private
/// until the shared `Display` for [`Guid`] lands; this keeps the test independent of it.)
fn text_of(guid: &cs2_dmx::Guid) -> String {
    let bytes = guid.0;
    format!(
        "{:08x}-{:04x}-{:04x}-{}-{}",
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_le_bytes([bytes[4], bytes[5]]),
        u16::from_le_bytes([bytes[6], bytes[7]]),
        bytes[8..10].iter().map(|b| format!("{b:02x}")).collect::<String>(),
        bytes[10..16].iter().map(|b| format!("{b:02x}")).collect::<String>(),
    )
}

fn element_id_of(dm: &Datamodel, guid: &cs2_dmx::Guid) -> cs2_dmx::ElementId {
    let index = dm.elements.iter().position(|element| element.id == *guid).expect("element");
    cs2_dmx::ElementId(index)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
