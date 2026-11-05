//! The same map in two encodings must decode to the same model.
//!
//! `cs2_map.vmap` / `cs2_map.vmap.txt` and `reflectiontest.vmap` / `reflectiontest.vmap.txt`
//! (in upstream's `Tests/Resources`, read in place, never copied: they are game content) are one
//! map each, once as binary 9 and once as keyvalues2 4. The two codecs were written apart; this
//! test is what ties them together. Elements are compared **by GUID** (the arenas are in
//! different orders): class, name, and every attribute, with element references mapped to the
//! GUID they name; the root GUID; the prefix attributes. Floats are compared bit for bit through
//! their `Debug` text (the shortest text that reads back as the same bits). The binary string
//! table is a binary-only artefact and is ignored.
//!
//! **The files are not identical saves**: see the two tests below for the exact, pinned list of
//! what differs (all of it is in the files, none in the codecs).
//!
//! Env-gated like the other real-file tests: without the files it prints `skipped: … missing`
//! and passes. `$CS2_MCP_PROJECT_ROOT` selects the checkout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cs2_dmx::{load, Attribute, Datamodel, ElementId, Guid};

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
    if dir.is_dir() {
        Some(dir)
    } else {
        println!("skipped: {} missing", dir.display());
        None
    }
}

/// An attribute as text with element references replaced by the GUID they name.
fn canonical(dm: &Datamodel, attribute: &Attribute) -> String {
    let guid = |id: &Option<ElementId>| match id {
        Some(id) => dm.elements[id.0].id.to_string(),
        None => "null".to_string(),
    };
    match attribute {
        Attribute::Element(target) => format!("Element({})", guid(target)),
        Attribute::ElementArray(items) => {
            format!("ElementArray[{}]", items.iter().map(guid).collect::<Vec<_>>().join(", "))
        }
        other => format!("{other:?}"),
    }
}

/// Every difference between two models.
///
/// Elements are paired by GUID. A GUID only one side has is not by itself a difference of the
/// models: Hammer mints fresh GUIDs for some elements on every export, so such an element is
/// paired with the element the other side holds at the same reference (the attribute that names
/// X on the left names Y on the right), and the pair is then compared like any other. What
/// cannot be paired is reported, and so is every attribute that differs once paired.
fn differences(left: &Datamodel, right: &Datamodel) -> Vec<String> {
    let mut out = Vec::new();
    let index = |dm: &Datamodel| -> BTreeMap<Guid, usize> {
        let mut map = BTreeMap::new();
        for (position, element) in dm.elements.iter().enumerate() {
            assert!(map.insert(element.id, position).is_none(), "duplicate GUID {}", element.id);
        }
        map
    };
    let (left_index, right_index) = (index(left), index(right));
    let prefix_left = left.prefix_element();
    let prefix_right = right.prefix_element();

    // left position -> right position
    let mut paired: BTreeMap<usize, usize> = BTreeMap::new();
    let mut taken: BTreeMap<usize, usize> = BTreeMap::new();
    let mut work: Vec<(usize, usize)> = Vec::new();
    let mut pair = |l: usize, r: usize, work: &mut Vec<(usize, usize)>, out: &mut Vec<String>| {
        match (paired.get(&l), taken.get(&r)) {
            (Some(&known), _) if known == r => {}
            (None, None) => {
                paired.insert(l, r);
                taken.insert(r, l);
                work.push((l, r));
            }
            _ => out.push(format!(
                "{} cannot pair with {} (already paired differently)",
                left.elements[l].id, right.elements[r].id
            )),
        }
    };
    for (guid, &l) in &left_index {
        if let Some(&r) = right_index.get(guid) {
            if Some(ElementId(l)) != prefix_left && Some(ElementId(r)) != prefix_right {
                pair(l, r, &mut work, &mut out);
            }
        }
    }
    if let (Some(l), Some(r)) = (left.root, right.root) {
        pair(l.0, r.0, &mut work, &mut out);
    } else if left.root != right.root {
        out.push("one model has no root".to_string());
    }

    let mut regenerated = 0;
    while let Some((l, r)) = work.pop() {
        let (le, re) = (&left.elements[l], &right.elements[r]);
        if le.id != re.id {
            regenerated += 1;
        }
        let label = format!("{} ({})", le.id, le.class_name);
        if le.class_name != re.class_name {
            out.push(format!("{label}: class {:?} vs {:?}", le.class_name, re.class_name));
        }
        if le.name != re.name {
            out.push(format!("{label}: name {:?} vs {:?}", le.name, re.name));
        }
        let names = |e: &cs2_dmx::Element| e.attributes.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        if names(le) != names(re) {
            out.push(format!("{label}: attribute names {:?} vs {:?}", names(le), names(re)));
            continue;
        }
        for ((name, la), (_, ra)) in le.attributes.iter().zip(&re.attributes) {
            // element references pair their targets; everything else is compared as text
            let targets = |attribute: &Attribute| match attribute {
                Attribute::Element(target) => Some(vec![*target]),
                Attribute::ElementArray(items) => Some(items.clone()),
                _ => None,
            };
            match (targets(la), targets(ra)) {
                (Some(lt), Some(rt)) => {
                    if lt.len() != rt.len() {
                        out.push(format!("{label}.{name}: {} references vs {}", lt.len(), rt.len()));
                        continue;
                    }
                    for (lt, rt) in lt.iter().zip(&rt) {
                        match (lt, rt) {
                            (Some(lt), Some(rt)) => pair(lt.0, rt.0, &mut work, &mut out),
                            (None, None) => {}
                            _ => out.push(format!("{label}.{name}: a null reference on one side only")),
                        }
                    }
                }
                _ => {
                    let (lt, rt) = (canonical(left, la), canonical(right, ra));
                    if lt != rt {
                        let clip = |s: &str| if s.len() > 120 { format!("{}...", &s[..120]) } else { s.to_string() };
                        out.push(format!("{label}.{name}: {} vs {}", clip(&lt), clip(&rt)));
                    }
                }
            }
        }
    }
    for (guid, &l) in &left_index {
        if !paired.contains_key(&l) && Some(ElementId(l)) != prefix_left {
            out.push(format!("{guid} ({}) is only in the left model", left.elements[l].class_name));
        }
    }
    for (guid, &r) in &right_index {
        if !taken.contains_key(&r) && Some(ElementId(r)) != prefix_right {
            out.push(format!("{guid} ({}) is only in the right model", right.elements[r].class_name));
        }
    }

    let describe = |dm: &Datamodel, id: Option<ElementId>| {
        id.map(|id| format!("{} ({})", dm.elements[id.0].id, dm.elements[id.0].class_name))
    };
    if describe(left, prefix_left) != describe(right, prefix_right) {
        out.push(format!(
            "prefix element {:?} vs {:?}",
            describe(left, prefix_left),
            describe(right, prefix_right)
        ));
    }
    let prefix = |dm: &Datamodel| dm.prefix_attributes.iter().map(|(n, a)| (n.clone(), canonical(dm, a))).collect::<Vec<_>>();
    let (pl, pr) = (prefix(left), prefix(right));
    let keys = |p: &[(String, String)]| p.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    if keys(&pl) != keys(&pr) {
        out.push(format!("prefix attribute names {:?} vs {:?}", keys(&pl), keys(&pr)));
    } else {
        for (l, r) in pl.iter().zip(&pr) {
            if l != r {
                out.push(format!("prefix attribute {}: {} vs {}", l.0, &l.1[..l.1.len().min(60)], &r.1[..r.1.len().min(60)]));
            }
        }
    }
    if (left.format.as_str(), left.format_version) != (right.format.as_str(), right.format_version) {
        out.push(format!("format {} {} vs {} {}", left.format, left.format_version, right.format, right.format_version));
    }
    out.push(format!("(regenerated GUIDs paired by reference: {regenerated})"));
    out
}

fn compare(binary: &str, text: &str, expected: &[&str]) {
    let Some(dir) = resources() else { return };
    let (binary_path, text_path) = (dir.join(binary), dir.join(text));
    if !binary_path.is_file() || !text_path.is_file() {
        println!("skipped: {} or {} missing", binary_path.display(), text_path.display());
        return;
    }
    let from_binary = load(&std::fs::read(&binary_path).unwrap()).unwrap();
    let from_text = load(&std::fs::read(&text_path).unwrap()).unwrap();
    assert_eq!(from_binary.encoding, "binary");
    assert_eq!(from_text.encoding, "keyvalues2");
    let diffs = differences(&from_binary, &from_text);
    println!(
        "{binary} vs {text}: {} elements vs {}, differences:\n  {}",
        from_binary.elements.len(),
        from_text.elements.len(),
        diffs.join("\n  ")
    );
    // The pinned list is every difference there is: a new one, or one that goes away, fails.
    assert_eq!(diffs, expected, "{binary} vs {text}");
}

/// The two `cs2_map` files are two different saves of one map, not two encodings of one save:
/// 43 elements with the same GUIDs, classes, names and attributes (every float bit for bit) except
/// that the text one has `mapUsageType` "hellyea" where the binary one has "standard", and the
/// prefix block is another one (another GUID, and the binary file carries the thumbnail where the
/// text file has no thumbnail at all). The codecs are not the cause: each file round-trips byte
/// for byte through its own codec.
#[test]
fn cs2_map_binary_and_text_differ_only_where_the_saves_do() {
    compare(
        "cs2_map.vmap",
        "cs2_map.vmap.txt",
        &[
            "c68a8842-922a-4a8a-ae05-268ea579658c (CMapWorld).mapUsageType: String(\"standard\") vs String(\"hellyea\")",
            "prefix element Some(\"e05aeb0f-68a8-4a3f-9a98-26f1472cb71f (DmElement)\") vs Some(\"baec7cd3-e2e3-4c81-890c-b7271d646563 ($prefix_element$)\")",
            "prefix attribute names [\"asset_preview_thumbnail\", \"asset_preview_thumbnail_format\", \"map_asset_references\"] vs [\"map_asset_references\"]",
            "(regenerated GUIDs paired by reference: 0)",
        ],
    );
}

/// `reflectiontest`: the same 72 elements, but 55 of them (the mesh data, plugs, ...) carry other
/// GUIDs in the text file than in the binary one; Hammer mints those afresh on an export. They pair
/// up one for one through the references and then agree on class, name and every attribute,
/// except one float: the text file spells `-3.0908623e-8` as `-0.0000000309` (ten decimals is all a
/// keyvalues2 float holds), which reads back as `-3.09e-8`. The prefix block has one GUID in both and
/// the class `$prefix_element$` only in the text file (the binary file calls it `DmElement`).
#[test]
fn reflectiontest_binary_and_text_differ_only_where_the_saves_do() {
    compare(
        "reflectiontest.vmap",
        "reflectiontest.vmap.txt",
        &[
            "b5496b5d-19c0-4cee-b377-18b2c30d70da (CStoredCamera).lookat: Vector3([-3.0908623e-8, -999.2929, 999.2929]) vs Vector3([-3.09e-8, -999.2929, 999.2929])",
            "prefix element Some(\"d5547f30-69f9-4f12-96c1-f45e95deb8fc (DmElement)\") vs Some(\"d5547f30-69f9-4f12-96c1-f45e95deb8fc ($prefix_element$)\")",
            "(regenerated GUIDs paired by reference: 55)",
        ],
    );
}

/// The check has teeth: a model compared with an edited copy of itself reports the edit.
#[test]
fn a_changed_attribute_is_reported() {
    let Some(dir) = resources() else { return };
    let path = dir.join("cs2_map.vmap.txt");
    if !path.is_file() {
        println!("skipped: {} missing", path.display());
        return;
    }
    let model = load(&std::fs::read(&path).unwrap()).unwrap();
    let mut edited = model.clone();
    let element = edited
        .elements
        .iter_mut()
        .find(|element| element.attributes.iter().any(|(_, a)| matches!(a, Attribute::Vector3(_))))
        .expect("the map has a vector3 somewhere");
    for (_, attribute) in element.attributes.iter_mut() {
        if let Attribute::Vector3(v) = attribute {
            v[0] += 1.0;
            break;
        }
    }
    assert!(differences(&model, &edited).iter().any(|d| d.contains(" vs ")));
    assert!(differences(&model, &model) == vec!["(regenerated GUIDs paired by reference: 0)".to_string()]);
}
