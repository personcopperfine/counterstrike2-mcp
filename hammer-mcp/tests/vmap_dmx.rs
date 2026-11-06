//! Golden tests for the Rust `.vmap` reader against the Python oracle
//! (the retired Python oracle `vmap_dmx.py`, `git show 8cdea42:bridge/vmapreader/vmap_dmx.py`), plus the required negative cases.
//!
//! The goldens are checked in as `fixtures/vmap/<name>.entities.json`; they are the oracle's own
//! `Entity.to_dict` records for every entity of the map, with keyvalues
//! (`fixtures/vmap/README.md` has the generator command). No python runs here: a missing or
//! unparsable fixture fails loudly instead of skipping.
//!
//! Both checked-in maps have empty `connectionsData` everywhere, so the I/O connection path is
//! covered by the `#[ignore]`d real-tree tests at the bottom
//! ([`real_tree_template_wingman_connections`], [`real_tree_cs_script_demo_connections`]): they
//! read the installed game's own `template_wingman.vmap` (one `logic_auto` connection) and
//! `cs_script_demo.vmap` (12 entities with connections), whose records were confirmed with the
//! oracle. Run them with `cargo test -p hammer-mcp --test vmap_dmx -- --ignored`.

use hammer_mcp::vmap::{load, VmapError};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PROBE_GOLDEN: &str = include_str!("../fixtures/vmap/probe.entities.json");
const DEFUSE_GOLDEN: &str = include_str!("../fixtures/vmap/defuse_poc.entities.json");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/vmap")
        .join(name)
}

fn golden(text: &str) -> Vec<Value> {
    let value: Value = serde_json::from_str(text).expect("golden JSON");
    value.as_array().expect("golden is an array").clone()
}

/// The golden record for one nodeID (the golden is sorted by nodeID).
fn golden_entity(golden: &[Value], node_id: i64) -> &Value {
    golden
        .iter()
        .find(|entity| entity["nodeID"].as_i64() == Some(node_id))
        .unwrap_or_else(|| panic!("golden has no nodeID {node_id}"))
}

fn node_ids(entities: &[&hammer_mcp::vmap::Entity]) -> Vec<i64> {
    let mut ids: Vec<i64> = entities
        .iter()
        .map(|entity| entity.node_id.expect("every fixture entity has a nodeID"))
        .collect();
    // find() keeps the file's element order (70 before 67 for the two buyzones); the goldens are
    // sorted by nodeID, so compare sorted.
    ids.sort_unstable();
    ids
}

/// Shared golden check: the reader's full JSON view of every entity equals the oracle's record.
fn check_golden(file: &str, text: &str) {
    let golden = golden(text);
    let document = load(&fixture(file)).unwrap_or_else(|error| panic!("{file}: {error}"));
    assert_eq!(
        document.entities().len(),
        golden.len(),
        "{file}: entity count differs from the oracle"
    );
    let mut seen = std::collections::HashSet::new();
    for entity in document.entities() {
        let node_id = entity.node_id.expect("fixture entities carry nodeID");
        assert!(seen.insert(node_id), "{file}: duplicate nodeID {node_id}");
        assert_eq!(
            &entity.to_json(true),
            golden_entity(&golden, node_id),
            "{file}: nodeID {node_id} differs from the oracle"
        );
    }
    // with_keyvalues = false drops only the keyvalues object
    for entity in document.entities() {
        let mut without = entity.to_json(false);
        let node_id = entity.node_id.unwrap();
        assert!(without.get("keyvalues").is_none(), "nodeID {node_id}");
        let full = entity.to_json(true);
        without.as_object_mut().unwrap().remove("keyvalues");
        let mut full = full;
        full.as_object_mut().unwrap().remove("keyvalues");
        assert_eq!(without, full, "nodeID {node_id}: shape without keyvalues");
    }
}

#[test]
fn probe_entities_match_the_oracle() {
    check_golden("probe.vmap", PROBE_GOLDEN);
}

#[test]
fn defuse_poc_entities_match_the_oracle() {
    check_golden("defuse_poc.vmap", DEFUSE_GOLDEN);
}

#[test]
fn worldspawn_is_node_id_one() {
    let document = load(&fixture("probe.vmap")).expect("probe.vmap");
    let world = document
        .get(Some(1), None)
        .expect("selector is valid")
        .expect("nodeID 1 exists");
    assert_eq!(world.classname, "worldspawn");
    assert_eq!(world.node_class, "CMapWorld");
    assert_eq!(world.origin, Some([0.0, 0.0, 0.0]));
    // nodeID wins over targetname, and an unmatched id is a plain None
    assert!(document.get(Some(999_999), None).expect("valid").is_none());
}

#[test]
fn find_by_class_uses_the_golden_counts() {
    let golden = golden(PROBE_GOLDEN);
    let expected: Vec<i64> = golden
        .iter()
        .filter(|entity| entity["classname"] == "func_buyzone")
        .map(|entity| entity["nodeID"].as_i64().unwrap())
        .collect();
    assert_eq!(expected, vec![67, 70], "golden func_buyzone nodeIDs");

    let document = load(&fixture("probe.vmap")).expect("probe.vmap");
    assert_eq!(
        node_ids(&document.find(Some("func_buyzone"), None, None, 100)),
        expected
    );
    // case-insensitive exact match, not a substring match
    assert_eq!(
        node_ids(&document.find(Some("FUNC_BUYZONE"), None, None, 100)),
        expected
    );
    assert!(document.find(Some("func_buy"), None, None, 100).is_empty());
    assert_eq!(document.find(None, None, None, 5).len(), 5);
    // targetname match, and the sphere filter on the node origin
    assert_eq!(
        node_ids(&document.find(None, Some("mcp_probe_1"), None, 100)),
        vec![22]
    );
    let near = document.find(None, None, Some(([96.0, -320.0, 32.0], 1.0)), 100);
    assert_eq!(node_ids(&near), vec![67]);
}

#[test]
fn get_by_targetname_and_invalid_selector() {
    let document = load(&fixture("probe.vmap")).expect("probe.vmap");
    let entity = document
        .get(None, Some("mcp_probe_1"))
        .expect("selector is valid")
        .expect("mcp_probe_1 exists");
    assert_eq!(entity.classname, "info_player_terrorist");
    assert!(document.get(None, Some("nope")).expect("valid").is_none());
    assert!(matches!(
        document.get(None, None),
        Err(VmapError::InvalidSelector)
    ));
}

#[test]
fn second_load_is_a_cache_hit() {
    let path = fixture("probe.vmap");
    let first: Arc<_> = load(&path).expect("probe.vmap");
    let second: Arc<_> = load(&path).expect("probe.vmap");
    assert!(Arc::ptr_eq(&first, &second), "cache miss on the second load");
    // a different path is a different entry
    let other = load(&fixture("defuse_poc.vmap")).expect("defuse_poc.vmap");
    assert!(!Arc::ptr_eq(&first, &other));
}

#[test]
fn element_index_matches_the_oracle_for_the_whole_file() {
    // `python3 -I vmap_dmx.py <file> info` (the retired oracle) on the same fixtures: the header,
    // prefix attributes and the full class histogram of the element index. This catches a stream
    // desync that the entity view alone could miss (it also proves the synthetic-stub path stayed
    // unused: no element is appended for either file).
    let data = std::fs::read(fixture("probe.vmap")).expect("probe.vmap");
    let document = hammer_mcp::vmap::dmx::parse(&data).expect("probe.vmap parses");
    assert_eq!(
        document.header,
        "<!-- dmx encoding binary 9 format vmap 40 -->\n"
    );
    assert_eq!(document.format, "vmap");
    assert_eq!(document.format_version, 40);
    assert_eq!(document.elements.len(), 626);
    assert!(document.elements.iter().all(|element| !element.stub));
    let prefix: Vec<&str> = document
        .prefix_attributes
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    // Prefix attributes stay in file order (`info` prints them sorted, so this order is the
    // reader's own, checked against the raw attribute names in the same order the oracle reads).
    assert_eq!(
        prefix,
        vec![
            "asset_preview_thumbnail",
            "asset_preview_thumbnail_format",
            "map_asset_references",
            "map_subasset_reference_types",
            "map_subasset_reference_names",
            "map_subasset_reference_counts",
        ]
    );
    let mut classes: Vec<(String, usize)> = Vec::new();
    for element in &document.elements {
        match classes.iter_mut().find(|(name, _)| *name == element.class_name) {
            Some((_, count)) => *count += 1,
            None => classes.push((element.class_name.clone(), 1)),
        }
    }
    classes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    assert_eq!(
        classes,
        vec![
            ("CDmePolygonMeshDataStream".to_string(), 198),
            ("DmElement".to_string(), 89),
            ("CDmePolygonMeshDataArray".to_string(), 72),
            ("DmePlugList".to_string(), 69),
            ("EditGameClassProps".to_string(), 69),
            ("CMapEntity".to_string(), 68),
            ("CDmePolygonMesh".to_string(), 18),
            ("CDmePolygonMeshSubdivisionData".to_string(), 18),
            ("CMapMesh".to_string(), 18),
            ("CMapRootElement".to_string(), 1),
            ("CMapSelectionSet".to_string(), 1),
            ("CMapVariableSet".to_string(), 1),
            ("CMapWorld".to_string(), 1),
            ("CStoredCamera".to_string(), 1),
            ("CStoredCameras".to_string(), 1),
            ("CVisibilityMgr".to_string(), 1),
        ]
    );
    // the element index order is the file's: root, the prefix-element duplicate, then the tree
    assert_eq!(document.elements[0].class_name, "CMapRootElement");
    assert_eq!(document.elements[0].guid, "faf1121d-10b5-4f7c-b855-4017911316f6");
    assert_eq!(document.elements[1].class_name, "DmElement");
    assert_eq!(document.elements[2].class_name, "CMapSelectionSet");
    assert_eq!(document.elements[368].class_name, "CMapWorld");
    assert_eq!(
        document.elements[368].guid,
        "640359e1-31ba-4869-a25f-1ff83bf4de4a"
    );
    assert_eq!(document.elements[368].attributes.len(), 20);

    let defuse = std::fs::read(fixture("defuse_poc.vmap")).expect("defuse_poc.vmap");
    assert_eq!(
        hammer_mcp::vmap::dmx::parse(&defuse)
            .expect("defuse_poc.vmap parses")
            .elements
            .len(),
        775
    );
}

#[test]
fn truncations_and_byte_flips_never_panic() {
    // Decoder-level sweep (no file I/O): every truncation must be an error, and a mutated byte
    // anywhere either parses or errors, but never panics/aborts.
    let data = std::fs::read(fixture("probe.vmap")).expect("probe.vmap");
    let mut cut = 0;
    while cut < data.len() {
        let error = hammer_mcp::vmap::dmx::parse(&data[..cut])
            .err()
            .unwrap_or_else(|| panic!("truncation at byte {cut} parsed successfully"));
        assert!(
            !error.to_string().contains("not implemented"),
            "truncation at {cut}: {error}"
        );
        cut += 4093;
    }
    for offset in [0, 8, 16, 64, 200, 4096, 100_000, 200_000, data.len() - 1] {
        for xor in [0x01u8, 0x40, 0x80, 0xff] {
            let mut mutated = data.clone();
            mutated[offset] ^= xor;
            let _ = hammer_mcp::vmap::dmx::parse(&mutated);
        }
    }
}

// ---------------------------------------------------------------- negative cases

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vmap-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn truncated_file_is_an_error_not_a_panic() {
    let data = std::fs::read(fixture("probe.vmap")).expect("probe.vmap");
    let cut = data.len() * 6 / 10;
    let path = scratch("truncated").join("probe-60pct.vmap");
    std::fs::write(&path, &data[..cut]).expect("write");
    let error = load(&path).expect_err("60% of the file must not parse");
    assert!(
        matches!(
            error,
            VmapError::Malformed { .. } | VmapError::TrailingData { .. }
        ),
        "unexpected error for a truncated file: {error}"
    );
}

#[test]
fn keyvalues2_encoding_is_unsupported() {
    let data = std::fs::read(fixture("probe.vmap")).expect("probe.vmap");
    let header_end = data.iter().position(|&b| b == 0).expect("header NUL");
    let header = String::from_utf8_lossy(&data[..header_end]).replace("binary", "keyvalues2");
    let mut rewritten = header.into_bytes();
    rewritten.push(0);
    rewritten.extend_from_slice(&data[header_end + 1..]);
    let path = scratch("keyvalues2").join("probe-keyvalues2.vmap");
    std::fs::write(&path, &rewritten).expect("write");
    assert!(matches!(
        load(&path),
        Err(VmapError::UnsupportedEncoding { found }) if found == "keyvalues2"
    ));
}

#[test]
fn appended_byte_is_trailing_data() {
    let mut data = std::fs::read(fixture("probe.vmap")).expect("probe.vmap");
    data.push(0);
    let path = scratch("trailing").join("probe-plus-one.vmap");
    std::fs::write(&path, &data).expect("write");
    assert!(matches!(
        load(&path),
        Err(VmapError::TrailingData { count: 1 })
    ));
}

#[test]
fn missing_file_is_an_io_error() {
    let path = scratch("missing").join("nope.vmap");
    match load(&path) {
        Err(VmapError::Io { path: reported, .. }) => assert_eq!(reported, path),
        other => panic!("expected VmapError::Io, got {other:?}"),
    }
}

// ---------------------------------------------------------------- allocation bounds

/// A minimal binary-9 stream: header NUL, `prefix elements` count, then whatever `tail` holds.
fn minimal_binary9(tail: &[u8]) -> Vec<u8> {
    let mut data = b"<!-- dmx encoding binary 9 format vmap 40 -->\n".to_vec();
    data.push(0);
    data.extend_from_slice(&0i32.to_le_bytes()); // v9 prefix elements
    data.extend_from_slice(tail);
    data
}

fn i32le(value: i32) -> [u8; 4] {
    value.to_le_bytes()
}

/// Brief item 14: a count read from the file must be bounded by the bytes left before it sizes an
/// allocation. A corrupt `i32::MAX` count in a 40-byte file must be an error, and the test running
/// under the agent's `ulimit -v` proves no multi-gigabyte `Vec` was reserved (`Vec::with_capacity`
/// of 2^31 items would abort).
#[test]
fn huge_counts_are_refused_without_allocating() {
    // the string table
    let data = minimal_binary9(&i32le(i32::MAX));
    let error = hammer_mcp::vmap::dmx::parse(&data).unwrap_err();
    assert!(
        error.to_string().contains("implausible string table count"),
        "{error}"
    );

    // the element index (a well-formed file needs 24 bytes per element)
    let mut tail = i32le(0).to_vec(); // string count 0
    tail.extend_from_slice(&i32le(i32::MAX)); // element count
    let error = hammer_mcp::vmap::dmx::parse(&minimal_binary9(&tail)).unwrap_err();
    assert!(error.to_string().contains("implausible element count"), "{error}");

    // a string index and an element index of one, then an attribute body with a huge count
    let mut tail = i32le(1).to_vec();
    tail.push(b'x'); // string 0: "x\0" (raw_str reads the NUL)
    tail.push(0);
    tail.extend_from_slice(&i32le(1)); // element count 1
    tail.extend_from_slice(&i32le(0)); // class name index
    tail.extend_from_slice(&i32le(0)); // element name index
    tail.extend_from_slice(&[0u8; 16]); // guid
    tail.extend_from_slice(&i32le(i32::MAX)); // attribute count for element 0
    let error = hammer_mcp::vmap::dmx::parse(&minimal_binary9(&tail)).unwrap_err();
    assert!(
        error.to_string().contains("implausible attribute count"),
        "{error}"
    );

    // ... and an array inside a valid attribute body
    let mut tail = i32le(1).to_vec();
    tail.push(b'x');
    tail.push(0);
    tail.extend_from_slice(&i32le(1)); // one element
    tail.extend_from_slice(&i32le(0)); // class name
    tail.extend_from_slice(&i32le(0)); // element name
    tail.extend_from_slice(&[0u8; 16]); // guid
    tail.extend_from_slice(&i32le(1)); // one attribute
    tail.extend_from_slice(&i32le(0)); // attribute name index
    tail.push(37); // string[] (type id 32 + 5)
    tail.extend_from_slice(&i32le(i32::MAX)); // the array count
    let error = hammer_mcp::vmap::dmx::parse(&minimal_binary9(&tail)).unwrap_err();
    assert!(error.to_string().contains("implausible array count"), "{error}");
}

/// The bound is a real limit, not a blanket refusal: a count that fits the remaining bytes still
/// parses (here: one element index row in a ~60-byte body).
#[test]
fn a_count_that_fits_the_remaining_bytes_still_parses() {
    let mut tail = i32le(1).to_vec();
    tail.push(b'x');
    tail.push(0);
    tail.extend_from_slice(&i32le(1));
    tail.extend_from_slice(&i32le(0));
    tail.extend_from_slice(&i32le(0));
    tail.extend_from_slice(&[0u8; 16]);
    tail.extend_from_slice(&i32le(0)); // no attributes
    let document = hammer_mcp::vmap::dmx::parse(&minimal_binary9(&tail)).expect("parses");
    assert_eq!(document.elements.len(), 1);
    assert_eq!(document.elements[0].class_name, "x");
}

// ---------------------------------------------------------------- `children_of` (WP-L)

/// The brush entities' `CMapMesh` children, by node id, in element order (probe.vmap: the two
/// `func_buyzone` -> meshes 66/71, the two `func_bomb_target` -> 31/32, the
/// `post_processing_volume` -> 75). The world's `children` array is the biggest one (81 rows,
/// world brushes first) and pins the element-order rule.
#[test]
fn children_of_returns_the_element_ordered_children() {
    let document = load(&fixture("probe.vmap")).expect("probe.vmap");
    let brush = |entity: &hammer_mcp::vmap::Entity| {
        entity
            .node_id
            .and_then(|id| document.children_of(id))
            .expect("every probe brush entity has a children entry")
            .to_vec()
    };
    for entity in document.entities() {
        match (entity.classname.as_str(), entity.node_id) {
            ("func_bomb_target", Some(33)) => {
                assert_eq!(brush(entity), vec![("CMapMesh".to_string(), Some(31))])
            }
            ("func_bomb_target", Some(34)) => {
                assert_eq!(brush(entity), vec![("CMapMesh".to_string(), Some(32))])
            }
            ("func_buyzone", Some(67)) => {
                assert_eq!(brush(entity), vec![("CMapMesh".to_string(), Some(66))])
            }
            ("func_buyzone", Some(70)) => {
                assert_eq!(brush(entity), vec![("CMapMesh".to_string(), Some(71))])
            }
            ("post_processing_volume", Some(74)) => {
                assert_eq!(brush(entity), vec![("CMapMesh".to_string(), Some(75))])
            }
            _ => {}
        }
    }

    // The world (nodeID 1) carries 81 children in element order: world brushes first, then the
    // entities/meshes of the tree (the oracle's walk). The brush entities' own meshes are NOT
    // world children — they hang off their entity.
    let world = document
        .children_of(1)
        .expect("the world owns a children array");
    assert_eq!(world.len(), 81);
    assert_eq!(world[0], ("CMapMesh".to_string(), Some(3)));
    assert_eq!(world[1], ("CMapEntity".to_string(), Some(4)));
    assert!(world.contains(&("CMapEntity".to_string(), Some(67))));
    assert!(world.contains(&("CMapEntity".to_string(), Some(70))));
    assert!(!world.contains(&("CMapMesh".to_string(), Some(66))));
}

/// `None` only for ids the document does not address; a point entity whose `children`
/// attribute exists but is empty reads as an empty list (its "no mesh children" answer), and
/// an unknown id none.
#[test]
fn children_of_unknown_or_childless_node_ids_are_none() {
    let document = load(&fixture("probe.vmap")).expect("probe.vmap");
    let terrorist = document
        .get(None, Some("mcp_probe_1"))
        .expect("selector is valid")
        .expect("mcp_probe_1 exists");
    assert_eq!(terrorist.classname, "info_player_terrorist");
    // nodeID 22 carries an empty `children` array: an honest empty list, not an unknown id.
    assert_eq!(
        document.children_of(22),
        Some(&[] as &[(String, Option<i64>)])
    );
    assert!(document.children_of(999_999).is_none(), "an unknown id");
    // defuse_poc carries the same brush shape (its own goldens pin the entity view).
    let defuse = load(&fixture("defuse_poc.vmap")).expect("defuse_poc.vmap");
    let bomb = defuse
        .get(Some(33), None)
        .expect("selector is valid")
        .expect("nodeID 33 exists");
    assert_eq!(bomb.classname, "func_bomb_target");
    assert_eq!(
        defuse.children_of(33).expect("brush entity"),
        [("CMapMesh".to_string(), Some(31))]
    );
}

// ---------------------------------------------------------------- the real game tree
// `#[ignore]`d: they need a CS2 install (Valve content, read-only). Run with
// `cargo test -p hammer-mcp --test vmap_dmx -- --ignored`.

/// `template_wingman.vmap`: 40 entities, exactly one of them (nodeID 73, a `logic_auto`) with a
/// single `OnMapSpawn` connection.
///
/// The expected record is the oracle's `Entity.connections` output (data values only, no file
/// bytes):
///
/// ```text
/// python3 -I vmap_dmx.py \
///   <install>/content/csgo/maps/templates/template_wingman.vmap find "logic_auto" --json
/// =>
/// {"outputName": "OnMapSpawn", "targetType": 7, "targetName": "script.2v2",
///  "inputName": "RunScriptCode", "overrideParam": "EnableWingman()", "delay": 0.0,
///  "timesToFire": -1}
/// ```
#[test]
#[ignore]
fn real_tree_template_wingman_connections() {
    let path = real_tree_file(TEMPLATE_WINGMAN);
    let document = load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert_eq!(
        document.entities().len(),
        40,
        "oracle: template_wingman has 40 entities"
    );

    // Cross-check against the oracle: one entity with connections, nodeID 73.
    let with_connections: Vec<i64> = with_connections(&document);
    assert_eq!(
        with_connections,
        vec![73],
        "oracle: exactly one entity carries connections"
    );
    // Everything else has an empty list, never a missing `connections` key.
    for entity in document.entities() {
        if entity.node_id != Some(73) {
            assert!(
                entity.connections.is_empty(),
                "nodeID {:?} unexpectedly carries {} connection(s)",
                entity.node_id,
                entity.connections.len()
            );
        }
    }

    let entity = document
        .get(Some(73), None)
        .expect("selector is valid")
        .expect("nodeID 73 exists");
    assert_eq!(entity.classname, "logic_auto");
    assert_eq!(
        entity.connections,
        vec![connection(
            "OnMapSpawn",
            7,
            "script.2v2",
            "RunScriptCode",
            "EnableWingman()",
            0.0,
            -1
        )]
    );
    // Types are the DMX ones: the ints are ints, `delay` is a float (not a string, not an int).
    let first = &entity.connections[0];
    for integer in ["targetType", "timesToFire"] {
        assert!(
            first[integer].is_i64(),
            "{integer} must be a JSON integer, got {}",
            first[integer]
        );
    }
    assert!(
        first["delay"].is_f64(),
        "delay must be a JSON float, got {}",
        first["delay"]
    );
    println!(
        "template_wingman.vmap: {} entities, {} with connections (nodeIDs {:?})",
        document.entities().len(),
        with_connections.len(),
        with_connections
    );
}

/// `cs_script_demo.vmap` (68 MB): 183 entities, 12 with connections. Spot-checks nodeID 296
/// (`trigger_multiple` "trigger.grenade") against the oracle, plus the 10-entry relay to prove the
/// file order and the string/int/float types survive.
#[test]
#[ignore]
fn real_tree_cs_script_demo_connections() {
    let path = real_tree_file(CS_SCRIPT_DEMO);
    let document = load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert_eq!(
        document.entities().len(),
        183,
        "oracle: cs_script_demo has 183 entities"
    );

    let with_connections = with_connections(&document);
    assert_eq!(
        with_connections,
        vec![296, 306, 349, 358, 383, 393, 423, 425, 430, 432, 434, 436],
        "oracle: the 12 entities with connections"
    );
    for entity in document.entities() {
        if !with_connections.contains(&entity.node_id.expect("nodeID")) {
            assert!(
                entity.connections.is_empty(),
                "nodeID {:?} unexpectedly carries connections",
                entity.node_id
            );
        }
    }

    // oracle: 296 trigger_multiple "trigger.grenade":
    // {outputName:"OnTrigger", targetType:7, targetName:"scriptent", inputName:"RunScriptInput",
    //  overrideParam:"GrenadeHit", delay:0.0, timesToFire:-1}
    let grenade = document
        .get(Some(296), None)
        .expect("selector is valid")
        .expect("nodeID 296 exists");
    assert_eq!(grenade.classname, "trigger_multiple");
    assert_eq!(grenade.targetname.as_deref(), Some("trigger.grenade"));
    assert_eq!(
        grenade.connections,
        vec![connection(
            "OnTrigger",
            7,
            "scriptent",
            "RunScriptInput",
            "GrenadeHit",
            0.0,
            -1
        )]
    );
    assert!(grenade.connections[0]["delay"].is_f64(), "delay is a float");

    // oracle: 358 logic_relay "relay.start_countdown" has 10 connections; entry 3 is the
    // `SetMessage "2"` at delay 1.0 and entry 9 the `board.timer` at delay 1.0 — the file order
    // (not sorted by delay) is what the reader must keep. `overrideParam` "2" is a *string*.
    let relay = document
        .get(Some(358), None)
        .expect("selector is valid")
        .expect("nodeID 358 exists");
    assert_eq!(relay.classname, "logic_relay");
    assert_eq!(relay.connections.len(), 10, "oracle: ten connections");
    assert_eq!(
        relay.connections[3],
        connection(
            "OnTrigger",
            7,
            "worldtext.training_countdown_dyn",
            "SetMessage",
            "2",
            1.0,
            -1
        )
    );
    assert_eq!(
        relay.connections[9],
        connection("OnTrigger", 7, "board.timer", "Enable", "", 1.0, -1)
    );
    assert!(
        relay.connections[3]["overrideParam"].is_string(),
        "a numeric-looking overrideParam stays a string: {}",
        relay.connections[3]["overrideParam"]
    );
    println!(
        "cs_script_demo.vmap: {} entities, {} with connections (nodeIDs {:?}); \
         nodeID 358 carries {} connections",
        document.entities().len(),
        with_connections.len(),
        with_connections,
        relay.connections.len()
    );
}

/// Every entity's nodeID that carries at least one connection, sorted ascending (the oracle output
/// is in element order; sorting makes the expected literal stable).
fn with_connections(document: &hammer_mcp::vmap::Document) -> Vec<i64> {
    let mut ids: Vec<i64> = document
        .entities()
        .iter()
        .filter(|entity| !entity.connections.is_empty())
        .map(|entity| entity.node_id.expect("every real-tree entity has a nodeID"))
        .collect();
    ids.sort_unstable();
    ids
}

/// One oracle `Entity.connections` record: the seven attributes in the oracle's key order.
#[allow(clippy::too_many_arguments)]
fn connection(
    output_name: &str,
    target_type: i64,
    target_name: &str,
    input_name: &str,
    override_param: &str,
    delay: f64,
    times_to_fire: i64,
) -> Map<String, Value> {
    json!({
        "outputName": output_name,
        "targetType": target_type,
        "targetName": target_name,
        "inputName": input_name,
        "overrideParam": override_param,
        "delay": delay,
        "timesToFire": times_to_fire,
    })
    .as_object()
    .expect("connection record is an object")
    .clone()
}

/// The project root: `$CS2_MCP_PROJECT_ROOT` first when it holds the tree, otherwise
/// found by walking up from the crate directory to the directory with
/// `workshop/run/app/game/csgo/csgo.fgd` (a worktree is nested several levels deeper than the main
/// checkout — the `tests/entity_classes.rs` / `tests/fgd.rs` rule). Never optional: a missing game
/// tree panics loudly instead of skipping the test.
fn project_root() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CS2_MCP_PROJECT_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        candidates.push(dir.clone());
        if !dir.pop() {
            break;
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(CSGO_FGD).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {CSGO_FGD} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

/// One read-only file of the real game tree; panics loudly when it is missing.
fn real_tree_file(relative: &str) -> PathBuf {
    let path = project_root().join(relative);
    assert!(
        path.is_file(),
        "missing real map {}: the installed game tree moved?",
        path.display()
    );
    path
}

/// The tree marker (same as the other real-tree tests).
const CSGO_FGD: &str = "workshop/run/app/game/csgo/csgo.fgd";
/// A Valve template map with exactly one I/O connection (nodeID 73).
const TEMPLATE_WINGMAN: &str = "workshop/run/app/content/csgo/maps/templates/template_wingman.vmap";
/// Valve's 68 MB scripting demo map (183 entities, 12 with connections).
const CS_SCRIPT_DEMO: &str =
    "workshop/run/app/content/csgo_addons/cs_script_demo/maps/cs_script_demo.vmap";
