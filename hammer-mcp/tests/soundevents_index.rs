//! The soundevent index (WP-S1, `src/soundevents/index.rs`) against the synthetic
//! [`common::SoundTree`] fixture (a csgo + core `pak01_dir.vpk` pair with three
//! `soundevents/*.vsndevts_c` blocks, an addon `.vsndevts` text and an addon `.vsndevts_c`) and,
//! behind `#[ignore]`, against the **real** workshop install.
//!
//! Covered: source precedence and the merge, the string-or-array normalisation,
//! `resolve_chain`'s base-first merge and its cycle bound, the suggestion ranking, `fresh()`'s
//! three `stat` classes, `rescan_addon` reusing the VPK half (and keeping the whole game half —
//! watches, sound assets and notes, review N1), the corrupt-block / absent-addon /
//! unparseable-source notes, the unparsed source never making its `_c` stale (review N3), and the
//! missing-VPK errors. The `list_soundevents` tool (filters, paging, the detail route, the argument
//! and not-found negatives) is `tests/soundevents_query.rs`.
#![allow(clippy::result_large_err)]

mod common;

use common::{
    bump_mtime, kv_object, kv_text, write_vsndevts_c, BuildFile, SoundTree, SND_CSGO_BLOCK,
    SND_CSGO_AMBIENCE_BLOCK, SND_CORE_BLOCK,
};
use hammer_mcp::soundevents::{IndexError, SoundEventIndex, SoundSource};
use serde_json::json;
use std::collections::BTreeMap;

/// The real workshop install the `#[ignore]`d corpus tests read: `<project-root>/workshop/run/app`
/// (`$CS2_MCP_PROJECT_ROOT`, default this checkout).
fn real_app_dir() -> std::path::PathBuf {
    let root = std::env::var("CS2_MCP_PROJECT_ROOT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/..").to_string());
    std::path::Path::new(&root).join("workshop/run/app")
}
/// The corpus fact the plan pins (183 csgo blocks, 21,601 events; the probe's `probe.out`).
const CORPUS_EVENTS: usize = 21_601;

/// Every event name the fixture carries (14: 6 in the weapons block, 4 ambience, 2 core, the
/// addon's compiled `mcp.both` + `Shared.Precedence` copy, the source-only `mcp.src.only`).
const ALL_NAMES: [&str; 14] = [
    "Shared.Precedence",
    "Weapon_AK47.Single",
    "amb.base",
    "amb.looping.atXYZ.base",
    "ambient_fx.child",
    "ambient_fx.child.a",
    "ambient_fx.child.b",
    "ambient_fx.orphan",
    "cycle.a",
    "cycle.b",
    "mcp.both",
    "mcp.src.only",
    "snd.near",
    "snd.near.long",
];

fn names(index: &SoundEventIndex) -> Vec<&str> {
    index
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect()
}

// -------------------------------------------------------------------------------------------
// precedence, merge and the read facts
// -------------------------------------------------------------------------------------------

#[test]
fn precedence_merge_and_the_read_facts() {
    let tree = SoundTree::new("precedence");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");

    assert_eq!(names(&index), ALL_NAMES, "sorted by name, one entry each");
    assert_eq!(index.files(), 5, "3 VPK blocks + the addon's _c and source");
    assert!(
        index.notes().is_empty(),
        "a complete fixture notes nothing: {:?}",
        index.notes()
    );

    // the addon's compiled copy outranks the csgo pak's, and the entry's facts are the addon's
    let shared = index.get("Shared.Precedence").expect("shared");
    assert_eq!(
        shared.sources,
        [SoundSource::AddonGame, SoundSource::VpkCsgo],
        "precedence order, best first"
    );
    assert!(shared.compiled);
    assert_eq!(
        shared.file,
        "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c",
        "the top-ranked copy's file"
    );
    assert_eq!(shared.volume, Some(0.25), "the addon's volume, not the pak's");
    assert_eq!(shared.source(), SoundSource::AddonGame);

    // an addon source-only name is compiled:false and carries one source
    let source_only = index.get("mcp.src.only").expect("mcp.src.only");
    assert_eq!(source_only.sources, [SoundSource::AddonSource]);
    assert!(!source_only.compiled);
    assert_eq!(
        source_only.file,
        "content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts"
    );
    assert_eq!(source_only.type_.as_deref(), Some("csgo_mega"));

    // an addon name with both sides: compiled, two sources, the compiled facts
    let both = index.get("mcp.both").expect("mcp.both");
    assert_eq!(
        both.sources,
        [SoundSource::AddonGame, SoundSource::AddonSource]
    );
    assert!(both.compiled);
    assert_eq!(both.volume, Some(0.9));

    // a VPK name: the internal block path, the pak named by `sources`
    let weapon = index.get("Weapon_AK47.Single").expect("weapon");
    assert_eq!(weapon.sources, [SoundSource::VpkCsgo]);
    assert_eq!(weapon.file, SND_CSGO_BLOCK);
    assert!(weapon.compiled);
    assert_eq!(weapon.type_.as_deref(), Some("csgo_mega"));
    assert_eq!(weapon.vsnds, ["sounds/weapons/ak47/ak47-1.vsnd"]);

    // the core pak (the second VPK, ranked after csgo)
    let template = index.get("amb.looping.atXYZ.base").expect("template");
    assert_eq!(template.sources, [SoundSource::VpkCore]);
    assert_eq!(template.file, SND_CORE_BLOCK);
    assert_eq!(template.base.as_deref(), Some("amb.base"));
    assert_eq!(template.position, Some([1.0, 2.0, 3.0]));
    assert_eq!(template.volume, Some(0.8));
    assert_eq!(
        index.get("amb.base").expect("base").type_.as_deref(),
        Some("csgo_ambient")
    );

    // the ambience block (a subdirectory of `soundevents/`)
    assert_eq!(
        index.get("ambient_fx.child.a").expect("child a").file,
        SND_CSGO_AMBIENCE_BLOCK
    );

    // WP-S2's gate: the union of the property keys actually seen
    let keys = index.known_property_keys();
    for key in [
        "type",
        "base",
        "volume",
        "position",
        "soundevent_01",
        "vsnd_files",
        "vsnd_files_track_01",
    ] {
        assert!(keys.contains(key), "{key} should be a known key: {keys:?}");
    }
    assert!(!keys.contains("not_a_real_key"));

    // the sound-asset path set backs the `known` verdicts
    assert_eq!(index.sound_asset_known("sounds/weapons/ak47/ak47-1.vsnd"), Some(true), "the pak's compiled spelling");
    assert_eq!(index.sound_asset_known("sounds\\only.vsnd"), Some(true), "the addon source, backslash spelling");
    assert_eq!(index.sound_asset_known("sounds/nope.vsnd"), Some(false));
    assert_eq!(index.sound_asset_known("/abs/x.vsnd"), None, "uncheckable");
    assert_eq!(index.sound_asset_known("sounds/../x.vsnd"), None, "uncheckable");
}

#[test]
fn string_or_array_normalisation() {
    let tree = SoundTree::new("normalise");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    // a string `vsnd_files_track_01` and an array `vsnd_files` on ONE event, in that order
    let child = index.get("ambient_fx.child").expect("ambient_fx.child");
    assert_eq!(
        child.vsnds,
        [
            "sounds/fx/fx_01.vsnd",
            "sounds/fx/a.vsnd",
            "sounds/fx/b.vsnd"
        ],
        "the string lane then the array lane"
    );
    assert_eq!(
        child.children,
        ["ambient_fx.child.a", "ambient_fx.child.b"],
        "soundevent_01 as a typed array"
    );
    // a string-only counterpart
    assert_eq!(
        index.get("Weapon_AK47.Single").expect("weapon").vsnds,
        ["sounds/weapons/ak47/ak47-1.vsnd"]
    );
    // an event with neither: two empty lists, never null
    let fallback = index.get("snd.near").expect("snd.near");
    assert!(fallback.vsnds.is_empty());
    assert!(fallback.children.is_empty());
    assert!(fallback.position.is_none());
    // `soundevent_01` in the STRING form is one child
    assert_eq!(
        index.get("ambient_fx.orphan").expect("orphan").children,
        ["ambient_fx.missing"]
    );
}

// -------------------------------------------------------------------------------------------
// resolve_chain
// -------------------------------------------------------------------------------------------

#[test]
fn resolve_chain_is_base_first_and_bounds_cycles() {
    let tree = SoundTree::new("chain");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");

    // base first, the named event last; the child's value wins in the merge
    let resolved = index
        .resolve_chain("amb.looping.atXYZ.base")
        .expect("known");
    assert_eq!(
        resolved.chain,
        ["amb.base", "amb.looping.atXYZ.base"],
        "base-first chain"
    );
    assert!(!resolved.cycle);
    assert_eq!(resolved.merged["volume"], json!(0.8), "the child overrides");
    assert_eq!(resolved.merged["position"], json!([1.0, 2.0, 3.0]));
    assert_eq!(
        resolved.merged["type"],
        json!("csgo_ambient"),
        "inherited from the base, which the child does not carry"
    );

    // a one-element chain for an event without a base
    let leaf = index.resolve_chain("amb.base").expect("amb.base");
    assert_eq!(leaf.chain, ["amb.base"]);
    assert!(!leaf.cycle);

    // a cycle: reported, and the chain stays bounded
    let cycle = index.resolve_chain("cycle.a").expect("cycle.a");
    assert!(cycle.cycle, "a -> b -> a must report the cycle");
    assert!(
        cycle.chain.len() <= 16,
        "the chain is bounded, got {}",
        cycle.chain.len()
    );
    assert!(cycle.chain.contains(&"cycle.a".to_string()));
    assert!(cycle.chain.contains(&"cycle.b".to_string()));
    for name in &cycle.chain {
        assert!(index.get(name).is_some(), "{name} is not an event");
    }

    // an unknown name has no resolution at all
    assert!(index.resolve_chain("nope.nope").is_none());
}

// -------------------------------------------------------------------------------------------
// suggestions
// -------------------------------------------------------------------------------------------

#[test]
fn suggestions_rank_the_case_insensitive_exact_match_first() {
    let tree = SoundTree::new("suggest");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");

    // `snd.near` shares the longer prefix but sorts before `snd.near.long`: the case-insensitive
    // exact match still comes first — that is the rule under test.
    assert_eq!(
        index.suggestions("SND.NEAR.LONG", 5),
        ["snd.near.long", "snd.near"]
    );
    assert_eq!(
        index.suggestions("Weapon_ak47.single", 5),
        ["Weapon_AK47.Single"]
    );
    assert_eq!(index.suggestions("Weapon_AK47.Singl", 5), ["Weapon_AK47.Single"]);
    // the request itself is never a suggestion, and a name sharing less than 3 characters is not
    // either
    assert!(index.suggestions("mcp.both", 5).iter().all(|s| s != "mcp.both"));
    assert_eq!(index.suggestions("zzz.zzz", 5), Vec::<String>::new());
    assert_eq!(index.suggestions("Weapon_AK47.Single", 0), Vec::<String>::new());
    // review L2: a whitespace-padded request gets its trimmed spelling (before the fix the
    // "the request itself is never a suggestion" rule swallowed it and the list came back empty)
    assert_eq!(index.suggestions("mcp.both ", 5), ["mcp.both", "mcp.src.only"]);
    assert_eq!(index.suggestions("  SND.NEAR.LONG\t", 5), ["snd.near.long", "snd.near"]);
}

// -------------------------------------------------------------------------------------------
// freshness and the addon rescan
// -------------------------------------------------------------------------------------------

#[test]
fn fresh_notices_both_addon_files_the_paks_and_the_directories() {
    let tree = SoundTree::new("fresh");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    assert!(index.fresh(), "a just-built index is fresh");

    // the addon's `.vsndevts` text
    bump_mtime(&tree.addon_source(), 5);
    assert!(!index.fresh(), "a touched addon source must not be fresh");

    // the addon's compiled `_c`
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("rebuild");
    assert!(index.fresh());
    bump_mtime(&tree.addon_compiled(), 5);
    assert!(!index.fresh(), "a recompiled addon block must not be fresh");

    // the game VPKs
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("rebuild");
    bump_mtime(&tree.csgo_vpk(), 5);
    assert!(!index.fresh(), "a moved csgo pak must not be fresh");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("rebuild");
    bump_mtime(&tree.core_vpk(), 5);
    assert!(!index.fresh(), "a moved core pak must not be fresh");

    // the directories themselves: a file appearing in (or vanishing from) one is noticed without
    // any recorded file's mtime changing
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("rebuild");
    std::fs::remove_dir_all(tree.addon_source_dir()).expect("remove the source dir");
    assert!(!index.fresh(), "a vanished addon directory must not be fresh");
}

#[test]
fn rescan_addon_sees_a_new_event_without_re_reading_the_vpk() {
    let tree = SoundTree::new("rescan");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    assert!(index.get("mcp.late").is_none());

    // The strongest possible "the VPK was not re-read" signal: the pak is GONE. A rescan that
    // re-opened it would have to fail (it returns `Self`) or lose the pak's events.
    std::fs::remove_file(tree.csgo_vpk()).expect("remove the csgo pak");
    assert!(!index.fresh(), "the deleted pak is a moved mtime");

    tree.add_source_file(
        "extra.vsndevts",
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.late\" =\n\t{\n\t\ttype = \"csgo_mega\"\n\t}\n}\n",
    );
    let rescanned = index.rescan_addon();
    assert!(
        rescanned.get("mcp.late").is_some(),
        "the rescan must pick up the new addon file"
    );
    assert!(
        rescanned.get("mcp.late").expect("late").sources == [SoundSource::AddonSource],
        "the new file is an addon-source copy"
    );
    assert!(
        rescanned.get("Weapon_AK47.Single").is_some(),
        "the pak's events survive the rescan although the pak file is gone"
    );
    assert_eq!(
        rescanned.get("Shared.Precedence").expect("shared").volume,
        Some(0.25),
        "the addon's copy still outranks the (reused) pak copy"
    );
    assert_eq!(rescanned.files(), 6, "the new source file joined the count");
}

// -------------------------------------------------------------------------------------------
// the negatives
// -------------------------------------------------------------------------------------------

#[test]
fn a_missing_game_vpk_is_an_error_naming_the_file() {
    let tree = SoundTree::new("no-csgo");
    std::fs::remove_file(tree.csgo_vpk()).unwrap();
    let error = SoundEventIndex::build(&tree.app(), &tree.addon).unwrap_err();
    let IndexError::VpkMissing { path, .. } = error else {
        panic!("expected VpkMissing, got {error:?}");
    };
    assert!(path.ends_with("game/csgo/pak01_dir.vpk"), "{path:?}");

    let tree = SoundTree::new("no-core");
    std::fs::remove_file(tree.core_vpk()).unwrap();
    let error = SoundEventIndex::build(&tree.app(), &tree.addon).unwrap_err();
    let IndexError::VpkMissing { path, .. } = error else {
        panic!("expected VpkMissing, got {error:?}");
    };
    assert!(path.ends_with("game/core/pak01_dir.vpk"), "{path:?}");
}

#[test]
fn a_corrupt_addon_block_is_a_note_and_the_rest_stands() {
    let tree = SoundTree::new("corrupt");
    tree.write_compiled(b"not a resource at all");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    let note = index
        .notes()
        .iter()
        .find(|note| note.contains("soundevents_addon.vsndevts_c"))
        .expect("the corrupt block is on the record");
    assert!(note.contains("not a compiled resource"), "{note}");
    // the other sources still answer, and the addon's own source copy of the corrupt file's
    // events remains (Shared.Precedence drops to the pak's copy)
    assert!(index.get("mcp.src.only").is_some());
    assert!(index.get("Weapon_AK47.Single").is_some());
    assert_eq!(index.get("mcp.both").expect("both").sources, [SoundSource::AddonSource]);
    assert_eq!(index.get("Shared.Precedence").expect("shared").volume, Some(0.5));

    // a container that parses but carries no DATA block is its own note
    let tree = SoundTree::new("no-data");
    tree.write_compiled(&common::model_container(&[("MDAT", b"x")]));
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    assert!(
        index
            .notes()
            .iter()
            .any(|note| note.contains("no DATA block")),
        "{:?}",
        index.notes()
    );

    // a DATA block that is not KV3 at all is a note too
    let tree = SoundTree::new("bad-data");
    tree.write_compiled(&common::model_container(&[("DATA", b"\x01\x02\x03\x04")]));
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    assert!(
        index
            .notes()
            .iter()
            .any(|note| note.contains("does not decode")),
        "{:?}",
        index.notes()
    );
    assert_eq!(names(&index).len(), 14, "the source copies still carry the addon names");

    // a rewritten compiled block replaces the old events (and the fixture-container helper is
    // accepted by the reader: one DATA block of KV3 v4)
    let tree = SoundTree::new("rewritten");
    tree.write_compiled(&write_vsndevts_c(&common::kv_object(vec![(
        "mcp.new",
        common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
    )])));
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    assert!(index.get("mcp.new").is_some());
    assert!(
        index.get("mcp.both").is_some(),
        "mcp.both survives through the addon source"
    );
    assert_eq!(
        index.get("mcp.both").expect("both").sources,
        [SoundSource::AddonSource],
        "the rewritten _c no longer carries it"
    );
}

#[test]
fn an_absent_addon_directory_is_a_note() {
    let tree = SoundTree::new("no-addon");
    std::fs::remove_dir_all(tree.addon_compiled_dir()).unwrap();
    std::fs::remove_dir_all(tree.addon_source_dir()).unwrap();
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    let notes = index.notes().join("\n");
    assert!(notes.contains("addon-game"), "{notes}");
    assert!(notes.contains("addon-source"), "{notes}");
    assert_eq!(notes.matches("does not exist").count(), 2, "{notes}");
    assert!(index.get("mcp.src.only").is_none());
    assert!(index.get("mcp.both").is_none());
    assert!(index.get("Weapon_AK47.Single").is_some(), "the paks still answer");
    assert_eq!(index.files(), 3, "only the three VPK blocks decoded");
}

#[test]
fn an_unparseable_source_is_a_note_with_line_and_column() {
    let tree = SoundTree::new("bad-source");
    // a header plus a member whose value is a stray `=` (the RESOURCE COMPILE ERROR shape)
    tree.write_source(
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.bad\" =\n\t{\n\t\tvolume = = 1\n\t}\n}\n",
    );
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    let note = index
        .notes()
        .iter()
        .find(|note| note.contains("soundevents_addon.vsndevts"))
        .expect("the parse failure is on the record");
    assert!(note.contains("could not be parsed"), "{note}");
    assert!(note.contains("line 5"), "{note}");
    assert!(note.contains("column"), "{note}");
    // the other sources still answer (the compiled addon event and every pak event)
    assert!(index.get("mcp.both").is_some());
    assert!(index.get("mcp.src.only").is_none(), "the broken text contributed nothing");
    assert!(index.get("Weapon_AK47.Single").is_some());
    assert_eq!(names(&index).len(), 13);
}

#[test]
fn a_source_without_a_header_is_a_note_at_line_one() {
    let tree = SoundTree::new("no-header");
    tree.write_source("{ \"mcp.x\" = { type = \"csgo_mega\" } }\n");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    let note = index
        .notes()
        .iter()
        .find(|note| note.contains("soundevents_addon.vsndevts"))
        .expect("noted");
    assert!(note.contains("line 1"), "{note}");
    assert!(note.contains("column 1"), "{note}");
}

// -------------------------------------------------------------------------------------------
// FIX-I: the M1 stale rule (index side) and the M3 rescan gate
// -------------------------------------------------------------------------------------------

/// Review M1: a name the addon's compiled `_c` carries while the `.vsndevts` source paired with
/// that `_c` is present and no longer carries it is **stale** (renamed or removed, not
/// recompiled) — `stale_source` names the source file. Everything else stays non-stale: a name
/// both sides carry, a name a game pak backs up (`sources != [addon-game]`), and a name whose
/// source file is gone (there is nothing to be stale against).
#[test]
fn a_compiled_name_its_source_dropped_is_stale() {
    let tree = SoundTree::new("stale");
    tree.write_compiled(&write_vsndevts_c(&common::kv_object(vec![
        (
            "mcp.gone",
            common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
        ),
        (
            "mcp.both",
            common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
        ),
        (
            "Shared.Precedence",
            common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
        ),
    ])));
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");

    let gone = index.get("mcp.gone").expect("mcp.gone");
    assert_eq!(gone.sources, [SoundSource::AddonGame], "the `_c` is the only holder");
    assert_eq!(
        index.stale_source(gone),
        Some("content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts"),
        "the source next to the `_c` no longer carries it"
    );
    assert_eq!(
        index.stale_source(index.get("mcp.both").expect("mcp.both")),
        None,
        "the source carries mcp.both too"
    );
    assert_eq!(
        index.stale_source(index.get("Shared.Precedence").expect("shared")),
        None,
        "the csgo pak carries the name as well (not addon-game alone)"
    );

    // the source file gone: no source to compare against, so not stale
    let tree = SoundTree::new("stale-no-source");
    std::fs::remove_file(tree.addon_source()).expect("remove the source");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    let both = index.get("mcp.both").expect("mcp.both");
    assert_eq!(both.sources, [SoundSource::AddonGame], "only the `_c` is left");
    assert_eq!(index.stale_source(both), None, "no source file to be stale against");
}

/// Review M3, the gate itself: `vpk_unchanged` is true right after a build, false as soon as either
/// pak's mtime moves, and false on the `Default` stub (no VPK half to reuse — a rebuild is the only
/// honest answer there).
#[test]
fn vpk_unchanged_gates_the_addon_rescan() {
    let tree = SoundTree::new("vpk-gate");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    assert!(index.vpk_unchanged(), "a just-built index has untouched paks");

    bump_mtime(&tree.csgo_vpk(), 5);
    assert!(!index.vpk_unchanged(), "a moved csgo pak forbids the rescan");
    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("rebuild");
    assert!(index.vpk_unchanged());

    bump_mtime(&tree.core_vpk(), 5);
    assert!(!index.vpk_unchanged(), "a moved core pak forbids the rescan");

    assert!(
        !SoundEventIndex::default().vpk_unchanged(),
        "the stub carries no VPK half: it must rebuild, never rescan from nothing"
    );
}

// -------------------------------------------------------------------------------------------
// FIX-J: the rescan keeps the whole game half (N1) and the unparsed source (N3)
// -------------------------------------------------------------------------------------------

/// Review N1: `assemble` used to `mem::take` the game half, so the `GamePart` stored on the index
/// was empty and every index built by [`SoundEventIndex::rescan_addon`] lost the VPK watches, the
/// game sound assets and the VPK notes. One `edit_soundevents` therefore made `fresh()` stop
/// watching the paks, made `vpk_unchanged()` false (the next edit re-decoded all 193 blocks) and
/// made every game `vsnds[].known` answer `false`; the non-event note went too. Everything below
/// is a piece of that half and must survive one rescan.
#[test]
fn rescan_addon_keeps_the_whole_game_half() {
    let tree = SoundTree::new("rescan-game-half");
    // a csgo pak the build writes a note about (a scalar root member plus a corrupt block) and
    // whose asset only the VPK carries
    let pak_root = kv_object(vec![
        (
            "pak.only",
            kv_object(vec![("type", kv_text("csgo_mega"))]),
        ),
        ("_auto_calc_vnsd_duration_", kv_text("1")),
    ]);
    common::write_vpk(
        &tree.app.join("game/csgo"),
        "pak01",
        2,
        &[
            BuildFile::loaded(SND_CSGO_BLOCK, &write_vsndevts_c(&pak_root), b"", true),
            BuildFile::loaded("soundevents/broken.vsndevts_c", b"not a resource", b"", true),
            BuildFile::preload_only("sounds/weapons/ak47/ak47-1.vsnd_c", b""),
        ],
    );
    // the tree's original pak kept its data in a sibling archive; the rewritten one is all in-dir
    let _ = std::fs::remove_file(tree.app.join("game/csgo/pak01_000.vpk"));

    let index = SoundEventIndex::build(&tree.app(), &tree.addon).expect("build");
    assert!(index.get("pak.only").is_some());
    assert!(index.get("mcp.both").is_some(), "the addon side is there too");
    let block_note = index
        .notes()
        .iter()
        .find(|note| note.contains("broken.vsndevts_c"))
        .expect("the VPK's corrupt block is on the record")
        .clone();
    assert!(block_note.contains("not a compiled resource"), "{block_note}");
    let non_event_note = index
        .notes()
        .iter()
        .find(|note| note.contains("are not event objects"))
        .expect("the VPK's scalar root member is on the record")
        .clone();
    assert!(non_event_note.contains("_auto_calc_vnsd_duration_"), "{non_event_note}");
    assert_eq!(
        index.sound_asset_known("sounds/weapons/ak47/ak47-1.vsnd"),
        Some(true),
        "the game asset backs `vsnds[].known`"
    );
    assert!(index.vpk_unchanged());
    assert!(index.fresh());

    tree.add_source_file(
        "extra.vsndevts",
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.late\" =\n\t{\n\t\ttype = \"csgo_mega\"\n\t}\n}\n",
    );
    let rescanned = index.rescan_addon();

    assert!(rescanned.get("mcp.late").is_some(), "the rescan saw the new addon file");
    assert!(rescanned.get("pak.only").is_some(), "the pak's events survive");
    assert!(
        rescanned.vpk_unchanged(),
        "N1: the rescan keeps the VPK watches (a second edit may rescan again)"
    );
    assert!(
        rescanned.fresh(),
        "N1: the rescanned index is fresh — it still watches the paks"
    );
    assert_eq!(
        rescanned.sound_asset_known("sounds/weapons/ak47/ak47-1.vsnd"),
        Some(true),
        "N1: the game's sound assets survive the rescan"
    );
    assert!(
        rescanned.notes().iter().any(|note| note == &block_note),
        "N1: the VPK note survives the rescan: {:?}",
        rescanned.notes()
    );
    assert!(
        rescanned.notes().iter().any(|note| note == &non_event_note),
        "N1: the non-event root-member note survives the rescan: {:?}",
        rescanned.notes()
    );
}

// -------------------------------------------------------------------------------------------
// the corpus (the real install): #[ignore]d, they FAIL (never pass) when the tree is absent
// -------------------------------------------------------------------------------------------

/// The real tree is required — a missing one is a failure with the path, never a pass.
fn real_app() -> std::path::PathBuf {
    let app = real_app_dir();
    assert!(
        app.join("game/csgo/pak01_dir.vpk").is_file(),
        "the real game tree is absent: {} (this test must not pass)", app.display()
    );
    assert!(app.join("game/core/pak01_dir.vpk").is_file());
    app
}

/// (a) the corpus: 193 blocks decode, the event count matches the probe, the `amb.*` templates
/// resolve (recording WHICH file holds them — plan assumption A4) and the addon's event is a
/// compiled one.
#[test]
#[ignore = "reads the real workshop install"]
fn corpus_decodes_the_real_tree() {
    let app = real_app();
    let index = SoundEventIndex::build(&app, "mcp_test").expect("build the real corpus");
    let mut per_source: BTreeMap<&'static str, usize> = BTreeMap::new();
    for entry in index.entries() {
        *per_source.entry(entry.source().as_str()).or_default() += 1;
    }
    eprintln!(
        "corpus: files={} events={} builtMs={} notes={} perTopSource={per_source:?}",
        index.files(),
        index.len(),
        index.built_ms(),
        index.notes().len()
    );
    for note in index.notes() {
        eprintln!("  note: {note}");
    }
    // Measured 2026-10-16: 193 corpus blocks (183 csgo + 10 core) + the addon's `_c` and its
    // `.vsndevts` = 195 decoded files, 21,862 event names (21,601 in the csgo pak per
    // target/orch/re/probe.out, plus the core pak and the mcp_test addon). The addon count is NOT
    // pinned exactly: the orchestrator's live acceptance runs `edit_soundevents` on this same
    // seeded file (it added `mcp.snd.timer` -> 21,863 measured 2026-10-16 after the live pass), so
    // the assertion is the floor that catches a degenerate index.
    assert_eq!(
        index.files(),
        195,
        "the 193 corpus blocks plus the addon's two files must decode"
    );
    assert!(
        index.len() >= 21_862,
        "the measured corpus event count must not shrink, got {}",
        index.len()
    );
    assert!(
        index.len() > CORPUS_EVENTS,
        "the corpus carries at least {CORPUS_EVENTS} events"
    );

    // A4: where do the `amb.*.base` templates live? (The plan expected the core pak; the answer
    // is the mcp_test addon's own compiled file — the seeded template text carries the base
    // blocks verbatim, and neither `amb.*.base` name is in the core pak at all.)
    let parent = index
        .get("amb.soundscapeParent.base")
        .expect("the plan's A4 name must resolve");
    eprintln!(
        "A4: amb.soundscapeParent.base -> file={:?} sources={:?} compiled={} base={:?}",
        parent.file, parent.sources, parent.compiled, parent.base
    );
    assert_eq!(
        parent.file,
        "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c",
        "A4: the addon's compiled file holds the soundscape parent template"
    );
    assert_eq!(
        parent.sources,
        [SoundSource::AddonGame, SoundSource::AddonSource],
        "A4: the core pak does not carry it (no vpk:core in the sources)"
    );
    for name in ["amb.base", "amb.looping.atXYZ.base", "amb.soundscapeParent.base"] {
        let entry = index.get(name).unwrap_or_else(|| panic!("{name} resolves"));
        eprintln!(
            "A4: {name} -> file={:?} sources={:?}",
            entry.file, entry.sources
        );
    }
    let chain = index
        .resolve_chain("amb.looping.atXYZ.base")
        .expect("the looping template resolves");
    eprintln!("A4: amb.looping.atXYZ.base chain={:?}", chain.chain);
    assert!(
        chain.chain.contains(&"amb.base".to_string()),
        "the chain reaches its base: {:?}",
        chain.chain
    );
    // What the plan's A4 candidate file (the core pak's 901-byte `soundevents_addon.vsndevts_c`)
    // actually carries — it is NOT where the `amb.*` templates are.
    let core_addon: Vec<&str> = index
        .entries()
        .iter()
        .filter(|entry| {
            entry.source() == SoundSource::VpkCore
                && entry.file == "soundevents/soundevents_addon.vsndevts_c"
        })
        .map(|entry| entry.name.as_str())
        .collect();
    eprintln!(
        "A4: the core pak's soundevents_addon block carries {} event(s): {core_addon:?}",
        core_addon.len()
    );
    assert!(
        !core_addon.contains(&"amb.base"),
        "the plan's candidate file does not carry amb.base"
    );

    // the seeded addon event: compiled, from both sides
    let point = index
        .get("mcp.snd.point")
        .unwrap_or_else(|| panic!("mcp.snd.point is in the mcp_test addon"));
    eprintln!(
        "mcp.snd.point: file={:?} sources={:?} compiled={} base={:?} vsnds={:?}",
        point.file, point.sources, point.compiled, point.base, point.vsnds
    );
    assert!(point.compiled, "the addon's _c carries mcp.snd.point");
    assert_eq!(
        point.sources,
        [SoundSource::AddonGame, SoundSource::AddonSource],
        "both sides carry it"
    );
    assert_eq!(point.base.as_deref(), Some("amb.looping.atXYZ.base"));

    // FIX-I M1 on the real tree: which names are stale (`addon-game` alone, the paired source no
    // longer carrying them)? The seeded addon's `_c` was compiled from the source that still holds
    // every name, so there must be none — a false positive here would warn on the live
    // `mcp.snd.*` checks.
    let stale: Vec<(&str, &str)> = index
        .entries()
        .iter()
        .filter_map(|entry| index.stale_source(entry).map(|file| (entry.name.as_str(), file)))
        .collect();
    eprintln!("M1: {} stale addon name(s): {stale:?}", stale.len());
    assert!(
        stale.is_empty(),
        "the real mcp_test addon is compiled from its source: {stale:?}"
    );
    assert!(
        index.known_property_keys().len() > 100,
        "the corpus defines the property-key universe"
    );
    assert!(index.known_property_keys().contains("override_dsp_preset"));

    // the suggestions and the cross-check that the index is not degenerate
    assert!(index.get("Weapon_AK47.Single").is_some());
    assert!(index.get("amb.base").is_some());
}

/// (b) the build cost (plan assumption A2): debug builds must stay under the 5 s budget; the
/// numbers (time, entries, the `properties` JSON size) go into AGENT.md.
#[test]
#[ignore = "reads the real workshop install"]
fn corpus_build_time_and_memory() {
    let app = real_app();
    let started = std::time::Instant::now();
    let index = SoundEventIndex::build(&app, "mcp_test").expect("build the real corpus");
    let wall_ms = started.elapsed().as_millis() as u64;
    let json_bytes = index.properties_json_bytes();
    eprintln!(
        "A2: builtMs={} wallMs={} events={} files={} propertiesJson={} bytes ({:.1} MiB)",
        index.built_ms(),
        wall_ms,
        index.len(),
        index.files(),
        json_bytes,
        json_bytes as f64 / (1024.0 * 1024.0)
    );
    assert!(
        index.built_ms() < 5000,
        "the debug build must stay under 5s, took {} ms",
        index.built_ms()
    );
    assert!(index.len() >= CORPUS_EVENTS);
}
