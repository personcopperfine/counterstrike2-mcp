//! `list_soundevents` (WP-S1, `src/tools/soundevents_query.rs`) against the synthetic
//! [`common::SoundTree`] fixture, through the real `HammerState` cache (no bridge is involved:
//! the tool is pure filesystem).
//!
//! Covered: the list filters and their composition, paging and the `truncated` arithmetic,
//! `summary.bySource`/`byType`, the `name` detail route (`resolved.chain`, the merge override
//! order, the `children`/`vsnds` verdicts) and its conflict with `query`, `index.rebuilt` across
//! the cache, the FIX-I/J rules (the stale verdict, the rescan-after-invalidate, the padded name,
//! the unparsed source never being stale), and every negative: the argument bounds, the unknown
//! source/argument, the unknown `name` with suggestions, the missing game VPK, and the note-only
//! failures (a corrupt addon block, an absent addon directory, an unparseable `.vsndevts`).
#![allow(clippy::result_large_err)]

mod common;

use common::{bump_mtime, write_vsndevts_c, SoundTree};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::tools::{Budget, CallCtx};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};

fn config(tree: &SoundTree) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let root = tree.root.display().to_string();
    let given: Vec<&str> = vec![
        "--deadline-ms",
        "5000",
        "--tool-budget-ms",
        "45000",
        "--project-root",
        &root,
    ];
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

fn ctx() -> CallCtx {
    CallCtx {
        budget: Budget::new(45_000, 5_000),
    }
}

fn state_of(tree: &SoundTree) -> HammerState {
    HammerState::from_config(&config(tree)).expect("state")
}

/// One `list_soundevents` call (the handler is the entry point the tests exercise).
async fn call(state: HammerState, args: Value) -> Result<Value, cs2_mcp_common::tools::ToolError> {
    hammer_mcp::tools::soundevents_query::list_soundevents(state, ctx(), args)
        .await
        .map(|out| out.json)
}

fn listed(out: &Value) -> Vec<String> {
    out["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_string())
        .collect()
}

// -------------------------------------------------------------------------------------------
// the list route
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_list_defaults_to_every_event_with_a_summary() {
    let tree = SoundTree::new("list");
    let state = state_of(&tree);
    let out = call(state.clone(), json!({})).await.unwrap();

    assert_eq!(out["total"], json!(14));
    assert_eq!(out["returned"], json!(14));
    assert_eq!(out["offset"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["index"]["events"], json!(14));
    assert_eq!(out["index"]["files"], json!(5));
    assert_eq!(out["index"]["rebuilt"], json!(true), "the first call builds");
    assert!(out["index"]["builtMs"].is_u64());
    assert!(out["notes"].as_array().unwrap().is_empty());

    let events = out["events"].as_array().unwrap();
    assert_eq!(
        events[1],
        json!({
            "name": "Weapon_AK47.Single",
            "type": "csgo_mega",
            "base": null,
            "volume": 1.0,
            "source": "vpk:csgo",
            "sources": ["vpk:csgo"],
            "compiled": true,
            "file": common::SND_CSGO_BLOCK,
            "vsndCount": 1,
            "childCount": 0,
            "position": null,
        }),
        "the compact record is the wire contract"
    );
    // `includeProperties` is opt-in for the list route (the `name` route always carries it)
    assert!(events[1].get("properties").is_none());

    // summary.bySource counts an event once per source it appears in
    assert_eq!(
        out["summary"]["bySource"],
        json!({
            "addon-game": 2,
            "addon-source": 2,
            "vpk:core": 2,
            "vpk:csgo": 10,
        })
    );
    // byType: the most common first, alphabetical on a tie, `(none)` for a typeless event
    assert_eq!(
        out["summary"]["byType"],
        json!({"csgo_mega": 12, "csgo_ambient": 1, "(none)": 1})
    );

    // the cache serves the second call without rebuilding
    let out = call(state, json!({})).await.unwrap();
    assert_eq!(out["index"]["rebuilt"], json!(false));
}

#[tokio::test]
async fn include_properties_carries_the_raw_object() {
    let tree = SoundTree::new("props");
    let state = state_of(&tree);
    let out = call(
        state,
        json!({"name": "snd.near", "includeProperties": true}),
    )
    .await
    .unwrap();
    // the `name` route ignores list args entirely; it always carries the properties
    assert_eq!(out["event"]["properties"], json!({"type": "csgo_mega"}));
    let out = call(
        state_of(&tree),
        json!({"query": "snd.near", "includeProperties": true}),
    )
    .await
    .unwrap();
    assert_eq!(listed(&out), ["snd.near", "snd.near.long"]);
    for entry in out["events"].as_array().unwrap() {
        assert_eq!(entry["properties"], json!({"type": "csgo_mega"}));
    }
}

#[tokio::test]
async fn the_filters_compose() {
    let tree = SoundTree::new("filters");
    let state = state_of(&tree);

    // substring, case-insensitive
    let out = call(state.clone(), json!({"query": "ak47"})).await.unwrap();
    assert_eq!(listed(&out), ["Weapon_AK47.Single"]);
    // a glob over the whole name when `*`/`?` is present
    let out = call(state.clone(), json!({"query": "ambient_fx.*"})).await.unwrap();
    assert_eq!(
        listed(&out),
        [
            "ambient_fx.child",
            "ambient_fx.child.a",
            "ambient_fx.child.b",
            "ambient_fx.orphan"
        ]
    );
    let out = call(state.clone(), json!({"query": "snd.ne?r"})).await.unwrap();
    assert_eq!(listed(&out), ["snd.near"], "a `?` matches one character");
    // prefix
    let out = call(state.clone(), json!({"prefix": "MCP."})).await.unwrap();
    assert_eq!(listed(&out), ["mcp.both", "mcp.src.only"]);
    // source
    let out = call(state.clone(), json!({"source": "addon"})).await.unwrap();
    assert_eq!(
        listed(&out),
        ["Shared.Precedence", "mcp.both", "mcp.src.only"]
    );
    let out = call(state.clone(), json!({"source": "game"})).await.unwrap();
    assert_eq!(out["total"], json!(12));
    // type (exact) and file (substring, case-insensitive)
    let out = call(state.clone(), json!({"type": "csgo_ambient"})).await.unwrap();
    assert_eq!(listed(&out), ["amb.base"]);
    let out = call(state.clone(), json!({"type": "csgo_mega", "prefix": "cycle."})).await.unwrap();
    assert_eq!(listed(&out), ["cycle.a", "cycle.b"]);
    let out = call(state.clone(), json!({"file": "WEAPONS"})).await.unwrap();
    assert_eq!(
        listed(&out),
        [
            "Weapon_AK47.Single",
            "cycle.a",
            "cycle.b",
            "snd.near",
            "snd.near.long"
        ],
        "Shared.Precedence's top copy is the addon's, so a `weapons` file filter drops it"
    );
    let out = call(state.clone(), json!({"file": "ambience"})).await.unwrap();
    assert_eq!(out["total"], json!(4));
    // hasPosition
    let out = call(state.clone(), json!({"hasPosition": true})).await.unwrap();
    assert_eq!(listed(&out), ["amb.looping.atXYZ.base"]);
    // composition
    let out = call(
        state,
        json!({"source": "game", "query": "amb", "hasPosition": true}),
    )
    .await
    .unwrap();
    assert_eq!(listed(&out), ["amb.looping.atXYZ.base"]);
}

#[tokio::test]
async fn paging_and_the_bounds() {
    let tree = SoundTree::new("paging");
    let state = state_of(&tree);
    let out = call(state.clone(), json!({"limit": 3, "offset": 2})).await.unwrap();
    assert_eq!(out["total"], json!(14), "the total counts every match");
    assert_eq!(out["returned"], json!(3));
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(listed(&out), ["amb.base", "amb.looping.atXYZ.base", "ambient_fx.child"]);

    // a page past the end is empty and honest
    let out = call(state.clone(), json!({"offset": 100})).await.unwrap();
    assert_eq!(out["total"], json!(14));
    assert_eq!(out["returned"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    assert!(listed(&out).is_empty());
    // the last page reports truncated:false
    let out = call(state.clone(), json!({"limit": 100, "offset": 13})).await.unwrap();
    assert_eq!(out["returned"], json!(1));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(listed(&out), ["snd.near.long"]);
    // limit 1000 is the maximum and does not clip the fixture
    let out = call(state, json!({"limit": 1000})).await.unwrap();
    assert_eq!(out["returned"], json!(14));
}

// -------------------------------------------------------------------------------------------
// the detail route
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_name_route_resolves_a_chain_and_merges_base_first() {
    let tree = SoundTree::new("detail");
    let state = state_of(&tree);
    let out = call(state.clone(), json!({"name": "amb.looping.atXYZ.base"}))
        .await
        .unwrap();

    assert_eq!(out["event"]["name"], json!("amb.looping.atXYZ.base"));
    assert_eq!(out["event"]["base"], json!("amb.base"));
    assert_eq!(out["event"]["type"], json!(null), "the template has no own type");
    assert_eq!(out["event"]["position"], json!([1.0, 2.0, 3.0]));
    assert_eq!(
        out["event"]["properties"],
        json!({"base": "amb.base", "position": [1.0, 2.0, 3.0], "volume": 0.8})
    );
    assert_eq!(
        out["resolved"]["chain"],
        json!(["amb.base", "amb.looping.atXYZ.base"]),
        "base first, the named event last"
    );
    assert_eq!(out["resolved"]["cycle"], json!(false));
    assert_eq!(
        out["resolved"]["merged"],
        json!({
            "type": "csgo_ambient",
            "volume": 0.8,
            "base": "amb.base",
            "position": [1.0, 2.0, 3.0],
        }),
        "the base's type survives, the child's volume wins"
    );
    assert_eq!(out["index"]["rebuilt"], json!(true), "the first call builds");
    assert!(out["notes"].as_array().unwrap().is_empty());

    // the children and the vsnds with their verdicts
    let out = call(state.clone(), json!({"name": "ambient_fx.child"}))
        .await
        .unwrap();
    assert_eq!(
        out["children"],
        json!([
            {"name": "ambient_fx.child.a", "known": true},
            {"name": "ambient_fx.child.b", "known": true},
        ])
    );
    assert_eq!(
        out["vsnds"],
        json!([
            {"path": "sounds/fx/fx_01.vsnd", "known": false},
            {"path": "sounds/fx/a.vsnd", "known": false},
            {"path": "sounds/fx/b.vsnd", "known": false},
        ])
    );
    // a dangling child and a known asset
    let out = call(state.clone(), json!({"name": "ambient_fx.orphan"}))
        .await
        .unwrap();
    assert_eq!(
        out["children"],
        json!([{"name": "ambient_fx.missing", "known": false}])
    );
    let out = call(state.clone(), json!({"name": "Weapon_AK47.Single"}))
        .await
        .unwrap();
    assert_eq!(
        out["vsnds"],
        json!([{"path": "sounds/weapons/ak47/ak47-1.vsnd", "known": true}]),
        "the pak's compiled spelling answers the check"
    );
    let out = call(state, json!({"name": "mcp.src.only"})).await.unwrap();
    assert_eq!(
        out["vsnds"],
        json!([{"path": "sounds/only.vsnd", "known": true}]),
        "the addon's own sounds tree"
    );
    assert_eq!(out["event"]["compiled"], json!(false));
}

#[tokio::test]
async fn the_name_route_reports_a_cycle_bounded() {
    let tree = SoundTree::new("cycle");
    let state = state_of(&tree);
    let out = call(state, json!({"name": "cycle.a"})).await.unwrap();
    assert_eq!(out["resolved"]["cycle"], json!(true));
    let chain = out["resolved"]["chain"].as_array().unwrap();
    assert!(chain.len() <= 16, "the chain is bounded: {chain:?}");
    assert!(chain.iter().any(|name| name == "cycle.a"));
    assert!(chain.iter().any(|name| name == "cycle.b"));
}

#[tokio::test]
async fn rebuilt_follows_the_cache_and_the_mtimes() {
    let tree = SoundTree::new("rebuilt");
    let state = state_of(&tree);
    let out = call(state.clone(), json!({})).await.unwrap();
    assert_eq!(out["index"]["rebuilt"], json!(true));
    let out = call(state.clone(), json!({"name": "amb.base"})).await.unwrap();
    assert_eq!(out["index"]["rebuilt"], json!(false), "served from the cache");
    bump_mtime(&tree.addon_source(), 5);
    let out = call(state, json!({})).await.unwrap();
    assert_eq!(out["index"]["rebuilt"], json!(true), "a moved source rebuilds");
}

// -------------------------------------------------------------------------------------------
// the shared gates (index_for / check_sound_name)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn index_for_caches_and_invalidate_drops_it() {
    use hammer_mcp::soundevents::{index_for, index_for_report};
    let tree = SoundTree::new("gate-cache");
    let state = state_of(&tree);

    let (first, rebuilt) = index_for_report(&state).await.unwrap();
    assert!(rebuilt);
    let (second, rebuilt) = index_for_report(&state).await.unwrap();
    assert!(!rebuilt, "the cached index is served");
    assert!(std::sync::Arc::ptr_eq(&first, &second), "the same Arc");
    assert_eq!(first.len(), 14);

    // the state slot holds it, and the file writer's hook drops it
    assert!(state.soundevent_index.lock().unwrap().is_some());
    state.soundevent_index_invalidate();
    assert!(state.soundevent_index.lock().unwrap().is_none());
    let (third, rebuilt) = index_for_report(&state).await.unwrap();
    assert!(rebuilt, "an invalidated cache rebuilds");
    assert!(!std::sync::Arc::ptr_eq(&first, &third));

    // `index_for` is the same cache
    assert!(std::sync::Arc::ptr_eq(
        &third,
        &index_for(&state).await.unwrap()
    ));

    // a missing game VPK fails the gate the same way the tool does
    let tree = SoundTree::new("gate-missing");
    std::fs::remove_file(tree.csgo_vpk()).unwrap();
    let error = index_for(&state_of(&tree)).await.unwrap_err();
    assert_eq!(error.name, "VpkMissing", "{error}");
}

#[tokio::test]
async fn check_sound_name_answers_every_verdict() {
    use hammer_mcp::soundevents::{check_sound_name, SoundNameVerdict, SoundSource};
    let tree = SoundTree::new("gate-verdicts");
    let state = state_of(&tree);

    // a compiled pak event
    assert_eq!(
        check_sound_name(&state, "Weapon_AK47.Single").await.unwrap(),
        SoundNameVerdict::Known {
            name: "Weapon_AK47.Single".into(),
            source: SoundSource::VpkCsgo,
            file: common::SND_CSGO_BLOCK.into(),
        }
    );
    // an addon event: the addon's copy is the top-ranked one
    assert_eq!(
        check_sound_name(&state, "mcp.both").await.unwrap(),
        SoundNameVerdict::Known {
            name: "mcp.both".into(),
            source: SoundSource::AddonGame,
            file: "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c".into(),
        }
    );
    // source-only: present, but not compiled
    assert_eq!(
        check_sound_name(&state, "mcp.src.only").await.unwrap(),
        SoundNameVerdict::SourceOnly {
            file: "content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts".into(),
        }
    );
    // the empty value and the direct-file forms never reach the index
    for empty in ["", "   "] {
        assert_eq!(check_sound_name(&state, empty).await.unwrap(), SoundNameVerdict::Empty);
    }
    for path in [
        "sounds/ambient/dust2/cablestress_01.vsnd",
        "sounds\\x.wav",
        "Sound.mp3",
        "a/b",
    ] {
        assert_eq!(
            check_sound_name(&state, path).await.unwrap(),
            SoundNameVerdict::FilePath,
            "{path}"
        );
    }
    // the case-sensitive miss comes back with suggestions (A1: names are hashed, not folded)
    assert_eq!(
        check_sound_name(&state, "Weapon_ak47.single").await.unwrap(),
        SoundNameVerdict::Unknown {
            suggestions: vec!["Weapon_AK47.Single".into()],
        }
    );
    // the whole thing ran off one cached index
    let built = state
        .soundevent_index
        .lock()
        .unwrap()
        .clone()
        .expect("cached");
    assert_eq!(built.len(), 14);
}

/// `is_file_path` is the predicate behind the `FilePath` verdict (unit-level).
#[test]
fn is_file_path_recognises_the_direct_file_forms() {
    use hammer_mcp::soundevents::is_file_path;
    for yes in [
        "sounds/x.wav",
        "a/b",
        "x.vsnd",
        "x.vsnd_c",
        "X.MP3",
        "sounds\\y.vsnd",
    ] {
        assert!(is_file_path(yes), "{yes}");
    }
    for no in ["Weapon_AK47.Single", "amb.base", "", "mcp.snd.point"] {
        assert!(!is_file_path(no), "{no}");
    }
}

// -------------------------------------------------------------------------------------------
// FIX-I: the M1 stale verdict, the M3 rescan-after-invalidate and the L2 padded name
// -------------------------------------------------------------------------------------------

/// Review M1: when the addon's compiled `_c` still carries a name its `.vsndevts` source renamed
/// or removed, `check_sound_name` answers `Stale { file }` (a warning for the callers) instead of
/// the old `Known` — the stale `_c` makes the name look playable until the next compile, when it
/// silently disappears. A name both sides carry stays `Known`.
#[tokio::test]
async fn check_sound_name_reports_a_stale_compiled_name() {
    use hammer_mcp::soundevents::{check_sound_name, SoundNameVerdict};
    let tree = SoundTree::new("gate-stale");
    tree.write_compiled(&write_vsndevts_c(&common::kv_object(vec![
        (
            "mcp.gone",
            common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
        ),
        (
            "mcp.both",
            common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
        ),
    ])));
    let state = state_of(&tree);

    assert_eq!(
        check_sound_name(&state, "mcp.gone").await.unwrap(),
        SoundNameVerdict::Stale {
            file: "content/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts".into(),
        }
    );
    assert_eq!(
        check_sound_name(&state, "mcp.both").await.unwrap(),
        SoundNameVerdict::Known {
            name: "mcp.both".into(),
            source: hammer_mcp::soundevents::SoundSource::AddonGame,
            file: "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c".into(),
        },
        "the source still carries it: not stale"
    );
}

/// Review L2: a whitespace-padded name is not the name the engine hashes — the verdict is
/// `Unknown` **with** the trimmed spelling suggested (before the fix the suggestion list was
/// empty, because the trimmed name was skipped as "the request itself").
#[tokio::test]
async fn check_sound_name_suggests_the_trimmed_name_for_a_padded_request() {
    use hammer_mcp::soundevents::{check_sound_name, SoundNameVerdict};
    let tree = SoundTree::new("gate-padded");
    let state = state_of(&tree);

    for (padded, trimmed) in [
        ("Weapon_AK47.Single ", "Weapon_AK47.Single"),
        ("  Weapon_AK47.Single", "Weapon_AK47.Single"),
        ("amb.base\t", "amb.base"),
    ] {
        let verdict = check_sound_name(&state, padded).await.unwrap();
        let SoundNameVerdict::Unknown { suggestions } = verdict else {
            panic!("{padded:?} must be Unknown, got {verdict:?}");
        };
        assert_eq!(
            suggestions.first().map(String::as_str),
            Some(trimmed),
            "the trimmed exact match ranks first for {padded:?}: {suggestions:?}"
        );
    }
    // the exact, unpadded name is still the exact name (and never its own suggestion)
    assert_eq!(
        check_sound_name(&state, "amb.base").await.unwrap(),
        SoundNameVerdict::Known {
            name: "amb.base".into(),
            source: hammer_mcp::soundevents::SoundSource::VpkCore,
            file: common::SND_CORE_BLOCK.into(),
        }
    );
}

/// Install `path`'s mtime exactly, so "this file was replaced with different bytes" does not read
/// as "this file moved" to `fresh()`/`vpk_unchanged`.
fn set_mtime(path: &std::path::Path, when: std::time::SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("{} for mtime: {e}", path.display()))
        .set_modified(when)
        .unwrap_or_else(|e| panic!("set mtime of {}: {e}", path.display()));
}

/// Review M3: after `soundevent_index_invalidate` (the writer's hook) **and** an addon change, the
/// next `index_for` must rescan the addon, not re-decode the two game VPKs. The signal is a hard
/// one: the csgo pak is overwritten with garbage (its mtime restored, so "the paks are untouched"
/// holds) — a full `build` could only fail (`CatalogVpkError`) and would lose every pak event,
/// while the rescan keeps them all. A pak whose mtime actually moved still rebuilds in full.
#[tokio::test]
async fn index_for_rescans_the_addon_and_rebuilds_on_a_moved_pak() {
    use hammer_mcp::soundevents::index_for_report;
    use std::time::SystemTime;
    let tree = SoundTree::new("m3-rescan");
    let state = state_of(&tree);

    let (first, rebuilt) = index_for_report(&state).await.unwrap();
    assert!(rebuilt, "the first call builds");
    assert!(first.get("Weapon_AK47.Single").is_some());
    assert!(first.get("mcp.late").is_none());

    // an addon edit, and the writer's hook (edit_soundevents / compile_soundevents call it)
    tree.add_source_file(
        "extra.vsndevts",
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.late\" =\n\t{\n\t\ttype = \"csgo_mega\"\n\t}\n}\n",
    );
    state.soundevent_index_invalidate();
    assert!(
        state.soundevent_index.lock().unwrap().is_none(),
        "the slot is dropped — the parked index is what makes the rescan possible"
    );
    assert!(
        state.soundevent_index_last.lock().unwrap().is_some(),
        "invalidate parks the index for the rescan"
    );

    // the csgo pak is not a VPK any more, but its mtime is the one the index recorded
    let recorded: SystemTime = std::fs::metadata(tree.csgo_vpk())
        .unwrap()
        .modified()
        .unwrap();
    std::fs::write(tree.csgo_vpk(), b"garbage, not a vpk at all").unwrap();
    set_mtime(&tree.csgo_vpk(), recorded);

    let (second, rebuilt) = index_for_report(&state).await.unwrap();
    assert!(rebuilt);
    assert!(second.get("mcp.late").is_some(), "the addon rescan saw the new file");
    assert!(
        second.get("Weapon_AK47.Single").is_some(),
        "the pak's events survived: the VPKs were not re-decoded"
    );
    // FIX-J N1: the rescanned index still carries the whole game half — before the fix its
    // `GamePart` had been `mem::take`n, so the paks were no longer watched (a second edit could
    // only rebuild) and every game `vsnds[].known` answered `false`
    assert!(
        second.vpk_unchanged(),
        "N1: the rescan keeps the VPK watches (the next edit can rescan again)"
    );
    assert_eq!(
        second.sound_asset_known("sounds/weapons/ak47/ak47-1.vsnd"),
        Some(true),
        "N1: the game's sound assets survive the rescan"
    );
    assert!(
        state.soundevent_index_last.lock().unwrap().is_none(),
        "the parked index is consumed by the rescan"
    );

    // a pak that really moved: a full rebuild (the new pak's names only)
    common::write_vpk(
        &tree.app.join("game/csgo"),
        "pak01",
        2,
        &[common::BuildFile::loaded(
            common::SND_CSGO_BLOCK,
            &write_vsndevts_c(&common::kv_object(vec![(
                "mcp.only.in.the.new.pak",
                common::kv_object(vec![("type", common::kv_text("csgo_mega"))]),
            )])),
            b"",
            true,
        )],
    );
    bump_mtime(&tree.csgo_vpk(), 5);
    state.soundevent_index_invalidate();
    let (third, rebuilt) = index_for_report(&state).await.unwrap();
    assert!(rebuilt);
    assert!(
        third.get("mcp.only.in.the.new.pak").is_some(),
        "a moved pak is read from disk"
    );
    assert!(
        third.get("Weapon_AK47.Single").is_none(),
        "the old pak's events are gone: the rebuild re-read it"
    );

    // FIX-J N1, once more: a rebuild-from-disk index is a normal index, so the NEXT threat is also
    // answered by a rescan. Before the fix only the first rescan was (M3 worked once), and this
    // step — an addon change over a garbage pak whose recorded mtime is restored — would have
    // tried a full build and failed with `CatalogVpkError`.
    assert!(
        third.vpk_unchanged(),
        "the rebuild-from-disk index watches the new pak"
    );
    tree.add_source_file(
        "extra2.vsndevts",
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.later\" =\n\t{\n\t\ttype = \"csgo_mega\"\n\t}\n}\n",
    );
    let recorded: SystemTime = std::fs::metadata(tree.csgo_vpk())
        .unwrap()
        .modified()
        .unwrap();
    std::fs::write(tree.csgo_vpk(), b"garbage, not a vpk at all").unwrap();
    set_mtime(&tree.csgo_vpk(), recorded);
    state.soundevent_index_invalidate();
    let (fourth, rebuilt) = index_for_report(&state).await.unwrap();
    assert!(rebuilt);
    assert!(
        fourth.get("mcp.later").is_some(),
        "the second rescan saw the new addon file"
    );
    assert!(
        fourth.get("mcp.only.in.the.new.pak").is_some(),
        "N1: the paks were reused again, not re-decoded (a rebuild could only fail here)"
    );
}

// -------------------------------------------------------------------------------------------
// FIX-J N3: an unparsed `.vsndevts` never makes its compiled `_c` look stale
// -------------------------------------------------------------------------------------------

/// Review N3: `addon_source_files` was filled *before* the text was read and parsed, so a
/// `.vsndevts` with a syntax error counted as "present with zero copies" and every name that only
/// its `_c` carries was reported [`SoundNameVerdict::Stale`] — a false warning on exactly the file
/// the source failed to build from. An unparsed source is no evidence: the verdict is `Known`, the
/// stale rule skips the file, and the parse note says so.
#[tokio::test]
async fn an_unparsed_source_never_makes_its_compiled_file_stale() {
    use hammer_mcp::soundevents::{check_sound_name, SoundNameVerdict, SoundSource};
    let tree = SoundTree::new("gate-unparsed");
    // the addon source is replaced by a text that cannot parse (the `_c` next to it is untouched
    // and still carries mcp.both)
    tree.write_source(
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.bad\" =\n\t{\n\t\tvolume = = 1\n\t}\n}\n",
    );
    let state = state_of(&tree);

    assert_eq!(
        check_sound_name(&state, "mcp.both").await.unwrap(),
        SoundNameVerdict::Known {
            name: "mcp.both".into(),
            source: SoundSource::AddonGame,
            file: "game/csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts_c".into(),
        },
        "the broken source proves nothing: the `_c` copy is not stale"
    );
    let index = state
        .soundevent_index
        .lock()
        .unwrap()
        .clone()
        .expect("the check cached the index");
    assert_eq!(
        index.stale_source(index.get("mcp.both").expect("mcp.both")),
        None,
        "stale_source skips the source that did not parse"
    );
    let note = index
        .notes()
        .iter()
        .find(|note| note.contains("could not be parsed"))
        .expect("the parse failure is on the record");
    assert!(
        note.contains("staleness") && note.contains("unknown"),
        "the note must say staleness is unknown for that file: {note}"
    );
}

// -------------------------------------------------------------------------------------------
// negatives
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn bad_arguments_are_invalid_arguments() {
    let tree = SoundTree::new("badargs");
    let state = state_of(&tree);
    for (args, why) in [
        (json!({"limit": 0}), "limit 0"),
        (json!({"limit": 1001}), "limit over the max"),
        (json!({"limit": "5"}), "limit wrong type"),
        (json!({"limit": -1}), "limit negative"),
        (json!({"offset": -1}), "offset negative"),
        (json!({"offset": 1.5}), "offset non-integer"),
        (json!({"source": "nope"}), "unknown source"),
        (json!({"source": 3}), "source wrong type"),
        (json!({"hasPosition": "yes"}), "hasPosition wrong type"),
        (json!({"query": ""}), "empty query"),
        (json!({"prefix": ""}), "empty prefix"),
        (json!({"type": ""}), "empty type"),
        (json!({"file": ""}), "empty file"),
        (json!({"name": ""}), "empty name"),
        (json!({"name": 3}), "name wrong type"),
        (json!({"name": "amb.base", "query": "amb"}), "name + query together"),
        (json!({"sort": "asc"}), "unknown argument"),
        (json!([1, 2]), "arguments not an object"),
    ] {
        let error = call(state.clone(), args.clone()).await.unwrap_err();
        assert_eq!(error.type_, "InvalidArguments", "{args} in {why}: {error}");
        assert_eq!(error.name, "InvalidParams", "{args} in {why}");
        assert!(error.retry_safe, "{args} in {why}");
    }
}

#[tokio::test]
async fn an_unknown_name_is_not_found_with_suggestions() {
    let tree = SoundTree::new("notfound");
    let state = state_of(&tree);
    let error = call(state.clone(), json!({"name": "Weapon_ak47.single"}))
        .await
        .unwrap_err();
    assert_eq!(error.type_, "SoundEventNotFound", "{error}");
    assert_eq!(error.name, "SoundEventNotFound");
    assert_eq!(error.data["name"], json!("Weapon_ak47.single"));
    assert_eq!(
        error.data["didYouMean"],
        json!(["Weapon_AK47.Single"]),
        "the case-insensitive exact match ranks first"
    );
    assert_eq!(
        error.data["sourcesChecked"],
        json!(["addon-game", "addon-source", "vpk:csgo", "vpk:core"])
    );
    assert!(error.retry_safe);

    // a wholly unknown name: no suggestions, still a clean not-found
    let error = call(state, json!({"name": "no.such.event"})).await.unwrap_err();
    assert_eq!(error.type_, "SoundEventNotFound");
    assert!(error.data.get("didYouMean").is_none());
}

#[tokio::test]
async fn a_missing_game_vpk_is_never_an_empty_list() {
    let tree = SoundTree::new("vpk-missing");
    std::fs::remove_file(tree.csgo_vpk()).unwrap();
    let state = state_of(&tree);
    let error = call(state.clone(), json!({})).await.unwrap_err();
    assert_eq!(error.name, "VpkMissing", "{error}");
    assert!(
        error.message.contains("game/csgo/pak01_dir.vpk"),
        "the file is named: {error}"
    );
    assert!(error.data["path"].is_string());
    // the detail route fails the same way (never a fabricated event)
    let error = call(state, json!({"name": "Weapon_AK47.Single"}))
        .await
        .unwrap_err();
    assert_eq!(error.name, "VpkMissing");
}

#[tokio::test]
async fn a_corrupt_addon_block_is_a_note_and_the_other_sources_answer() {
    let tree = SoundTree::new("corrupt");
    tree.write_compiled(b"garbage, not a container");
    let state = state_of(&tree);
    let out = call(state, json!({})).await.unwrap();
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap()
            .contains("soundevents_addon.vsndevts_c")
            && note.as_str().unwrap().contains("not a compiled resource")),
        "{notes:?}"
    );
    // the VPK events and the addon source are still listed, and the corrupt file changed which
    // copy wins for the shared name
    assert_eq!(out["total"], json!(14));
    assert_eq!(out["index"]["files"], json!(4), "the corrupt block did not decode");
    let shared = out["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Shared.Precedence")
        .expect("Shared.Precedence");
    assert_eq!(shared["source"], json!("vpk:csgo"));
    assert_eq!(shared["volume"], json!(0.5));
}

#[tokio::test]
async fn an_absent_addon_directory_and_an_unparseable_source_are_notes() {
    let tree = SoundTree::new("notes");
    std::fs::remove_dir_all(tree.addon_compiled_dir()).unwrap();
    std::fs::remove_dir_all(tree.addon_source_dir()).unwrap();
    let out = call(state_of(&tree), json!({})).await.unwrap();
    let notes = out["notes"].as_array().unwrap();
    let joined = notes
        .iter()
        .map(|note| note.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("addon-game"), "{joined}");
    assert!(joined.contains("does not exist"), "{joined}");
    assert_eq!(out["index"]["files"], json!(3), "only the VPK blocks decoded");

    // an unparseable `.vsndevts` names line and column and the list still answers
    let tree = SoundTree::new("parse-error");
    tree.write_source(
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.bad\" =\n\t{\n\t\tvolume = = 1\n\t}\n}\n",
    );
    let out = call(state_of(&tree), json!({})).await.unwrap();
    let notes = out["notes"].as_array().unwrap();
    let note = notes
        .iter()
        .map(|note| note.as_str().unwrap())
        .find(|note| note.contains("soundevents_addon.vsndevts"))
        .expect("the parse error is on the record");
    assert!(note.contains("could not be parsed"), "{note}");
    assert!(note.contains("line 5"), "{note}");
    assert!(note.contains("column"), "{note}");
    assert_eq!(out["total"], json!(13), "the broken source contributed nothing");
    assert!(listed(&out).iter().all(|name| name != "mcp.src.only"));
}
