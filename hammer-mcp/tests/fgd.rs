//! Tests for the FGD parser / class database (WP-F): the hand-written fixture tree at
//! `fixtures/fgd/` (the mini game tree in `csgo/` + `core/`, the negative cases in `cases/`),
//! the trigger/spawnflag lookups of the trigger tools (WP-F) plus their required negative tests
//! (unterminated string/brace, include cycle, BadType/Forbidden issues, a root without csgo.fgd,
//! unknown spawnflag names/bits, non-integer spawnflags text, a class without spawnflags) and
//! `#[ignore]`d tests that parse the real game tree under the project root (run manually
//! with `-- --ignored`).

use cs2_mcp_common::config::CommonConfig;
use hammer_mcp::fgd::{
    ClassKind, Database, FgdError, IssueKind, Severity, SpawnflagsError, UnknownSpawnflag,
};
use hammer_mcp::state::HammerState;
use serde_json::{json, Map};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn root(rel: &str) -> PathBuf {
    fixtures().join(rel)
}

fn kv(entries: &[(&str, serde_json::Value)]) -> Map<String, serde_json::Value> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

// ---------------------------------------------------------------- the real fixture tree

#[test]
fn fixture_loads_with_two_files_and_expected_warnings() {
    let db = Database::load(&root("fgd")).unwrap();
    // Load order: the included core file first (the root is recorded before its includes? no —
    // the root is pushed first, then its includes recurse; assert both are present).
    let names: Vec<&str> = db.files.iter().map(|p| p.file_name().unwrap().to_str().unwrap()).collect();
    assert_eq!(names.len(), 2, "files: {names:?}");
    assert!(names.contains(&"csgo.fgd") && names.contains(&"base_mini.fgd"), "{names:?}");
    // Exactly the two deliberate duplicate-definition warnings, nothing else.
    let dups = db
        .warnings
        .iter()
        .filter(|w| w.contains("duplicate class 'info_mini'"))
        .count();
    assert_eq!(dups, 2, "warnings: {:#?}", db.warnings);
    assert_eq!(db.warnings.len(), 2, "warnings: {:#?}", db.warnings);
}

#[test]
fn info_target_resolves_base_chain_and_flags() {
    let db = Database::load(&root("fgd")).unwrap();
    let c = db.class("info_target").expect("info_target defined");
    assert_eq!(c.kind, ClassKind::Point);
    assert_eq!(c.bases, vec!["Targetname".to_string(), "Parentname".to_string()]);
    // Full base chain, depth-first: Targetname -> GameEntity, then Parentname.
    let chain: Vec<&str> = c.resolved_bases.iter().map(|s| s.as_str()).collect();
    assert_eq!(chain, vec!["Targetname", "GameEntity", "Parentname"]);
    // Child keys after inherited keys.
    let names: Vec<&str> = c.keys.iter().map(|k| k.name.as_str()).collect();
    assert!(
        names.iter().position(|n| *n == "spawnflags").unwrap()
            > names.iter().position(|n| *n == "targetname").unwrap(),
        "keys: {names:?}"
    );
    let sf = c.keys.iter().find(|k| k.name == "spawnflags").unwrap();
    assert_eq!(sf.type_, "flags");
    assert_eq!(sf.flags.len(), 2);
    assert_eq!(sf.flags[0].bit, 1);
    assert_eq!(sf.flags[1].bit, 2);
    assert!(!sf.flags[1].default_on);
    // Inherited key metadata: declared_in names the base.
    let tn = c.keys.iter().find(|k| k.name == "targetname").unwrap();
    assert_eq!(tn.declared_in, "Targetname");
    assert_eq!(tn.type_, "target_source");
    // Inherited inputs from GameEntity via Targetname.
    assert!(c.inputs.iter().any(|io| io.name == "Kill" && io.declared_in == "GameEntity"));
    // The description continuation is joined without the `+`.
    assert!(
        c.description.as_deref().unwrap().contains("does nothing. Very useful"),
        "desc: {:?}",
        c.description
    );
}

#[test]
fn resolution_is_case_insensitive_and_validate_matches_keys_case_insensitively() {
    let db = Database::load(&root("fgd")).unwrap();
    assert!(db.class("INFO_TARGET").is_some(), "lookup is case-insensitive");
    let v = db.validate("info_target", &kv(&[("TargetName", json!("my_target"))]));
    assert!(v.issues.is_empty(), "{v:?}");
}

#[test]
fn overridden_key_replaced_in_place_and_helpers_merge() {
    let db = Database::load(&root("fgd")).unwrap();
    let c = db.class("light_environment").unwrap();
    // The @OverrideClass replaced directlight in place (same position, new default + choices).
    let dl = c.keys.iter().find(|k| k.name == "directlight").unwrap();
    assert_eq!(dl.default.as_deref(), Some("3"));
    assert_eq!(dl.choices.len(), 4, "{:?}", dl.choices);
    assert_eq!(dl.declared_in, "light_environment");
    assert!(dl.type_ == "choices");
    // fog from the base definition survives; clientSideEntity is appended.
    assert!(c.keys.iter().any(|k| k.name == "fog" && k.type_ == "remove_key"));
    assert!(c.keys.iter().any(|k| k.name == "clientSideEntity"));
    // The metadata helper from the override is recorded.
    assert!(c.helpers.iter().any(|(n, args)| n == "metadata" && args.contains("entity_tool_name")));
}

#[test]
fn solid_class_and_meta_block_after_key_parse() {
    let db = Database::load(&root("fgd")).unwrap();
    let c = db.class("func_buyzone").unwrap();
    assert_eq!(c.kind, ClassKind::Solid);
    let names: Vec<&str> = c.inputs.iter().map(|io| io.name.as_str()).collect();
    assert!(names.contains(&"Enable") && names.contains(&"Disable") && names.contains(&"Kill"), "{names:?}");
    assert!(c.outputs.iter().any(|o| o.name == "BuyzoneEntered"));
    // The key with an empty default (`: :`) parsed with description only.
    let team = c.keys.iter().find(|k| k.name == "TeamNum").unwrap();
    assert_eq!(team.type_, "integer");
    assert_eq!(team.display_name.as_deref(), Some("Team Num"));
    assert_eq!(team.default, None);
    assert!(team.description.is_some());
    // A value list whose entries carry a description segment and a default bit.
    let bz = c.keys.iter().find(|k| k.name == "BuyzoneType").unwrap();
    assert_eq!(bz.type_, "choices");
    assert_eq!(bz.choices.len(), 2, "{:?}", bz.choices);
    assert_eq!(bz.choices[0].value, "0");
    assert_eq!(bz.choices[0].label, "Ground");
    assert_eq!(bz.choices[1].label, "Air");
    // A dangling `: ` at the end of a key must not eat the name of the next key (the
    // `core/lights2.fgd` form).
    let ff = c.keys.iter().find(|k| k.name == "fadefactor").unwrap();
    assert_eq!(ff.default.as_deref(), Some("1.0"));
    assert_eq!(ff.description, None);
    let br = c.keys.iter().find(|k| k.name == "BuyzoneRadius").unwrap();
    assert_eq!(br.type_, "float");
    assert_eq!(br.default.as_deref(), Some("64"));
}

#[test]
fn duplicate_class_later_definition_wins() {
    let db = Database::load(&root("fgd")).unwrap();
    let c = db.class("info_mini").unwrap();
    // csgo.fgd's definition is last -> its key set wins entirely.
    assert_eq!(c.description.as_deref(), Some("Second definition wins."));
    assert!(c.keys.iter().any(|k| k.name == "second_wave"));
    assert!(c.source.contains("csgo.fgd:"), "source: {}", c.source);
}

#[test]
fn unparseable_class_is_skipped_with_warning_not_error() {
    let db = Database::load(&root("fgd/cases/badclass")).unwrap();
    assert!(
        db.class("still_parses").is_some(),
        "the class after the broken one must still load"
    );
    assert!(
        db.warnings.iter().any(|w| w.contains("csgo.fgd:2:") && w.contains("broken")),
        "warnings: {:#?}",
        db.warnings
    );
    assert!(db.class("never_reached").is_none());
}

#[test]
fn validate_unknown_class_gives_sorted_nearest_names() {
    let db = Database::load(&root("fgd")).unwrap();
    let v = db.validate("info_targt", &Map::new());
    assert!(v.unknown_class);
    assert!(
        v.nearest.iter().any(|n| n == "info_target"),
        "nearest: {:?}",
        v.nearest
    );
    assert!(v.issues.is_empty());
}

#[test]
fn validate_unknown_key_and_accepted_engine_keys() {
    let db = Database::load(&root("fgd")).unwrap();
    let v = db.validate(
        "info_target",
        &kv(&[("spawnflags", json!("4")), ("bogus", json!("x"))]),
    );
    assert!(!v.unknown_class);
    assert_eq!(v.issues.len(), 1, "{v:?}");
    let issue = &v.issues[0];
    assert_eq!(issue.key, "bogus");
    assert_eq!(issue.kind, IssueKind::UnknownKey);
    assert!(issue.message.contains("bogus"), "{issue:?}");
    // spawnflags "4" produced no issue.

    // Engine-managed keys are never flagged, even on a class that does not declare them.
    let v = db.validate(
        "info_target",
        &kv(&[("origin", json!("0 0 0")), ("angles", json!("0 0 0"))]),
    );
    assert!(v.issues.is_empty(), "{v:?}");

    // The editor-added `local.*` family is NOT whitelisted globally: it is declared by the
    // Parentname/WeaponCore bases in the real trees, so a class that does not inherit it must be
    // told about it (brief item 12).
    let v = db.validate("info_target", &kv(&[("local.origin", json!("0 0 0"))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    assert_eq!(v.issues[0].kind, IssueKind::UnknownKey);
    assert_eq!(v.issues[0].severity, Severity::Error);
}

/// Brief item 12: an integral float on an integer key is accepted by Hammer, so it is a *warning*
/// (reported, never refused by `strict`), while a fractional value stays an error.
#[test]
fn validate_integral_float_on_an_integer_key_is_a_warning() {
    let db = Database::load(&root("fgd")).unwrap();
    let v = db.validate("func_buyzone", &kv(&[("TeamNum", json!(4.0))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    let issue = &v.issues[0];
    assert_eq!(issue.kind, IssueKind::BadType);
    assert_eq!(issue.severity, Severity::Warning, "{issue:?}");
    assert!(issue.message.contains("integral float"), "{issue:?}");
    assert!(!v.has_errors(), "strict must not refuse this: {v:?}");

    // the same text as a string, and a negative integer text, are clean
    for good in [json!("4.0"), json!("-4"), json!("4")] {
        let v = db.validate("func_buyzone", &kv(&[("TeamNum", good.clone())]));
        assert!(
            !v.has_errors(),
            "{good} must not be an error: {v:?}"
        );
    }
    // a fractional value is still an error, and so is text
    for bad in [json!(4.5), json!("abc")] {
        let v = db.validate("func_buyzone", &kv(&[("TeamNum", bad.clone())]));
        assert_eq!(v.issues.len(), 1, "{bad}: {v:?}");
        assert_eq!(v.issues[0].severity, Severity::Error, "{bad}: {v:?}");
        assert!(v.has_errors(), "{bad}");
    }
    // a warning-severity message never sets has_errors on its own
    let v = db.validate("light_environment", &kv(&[("_fademindist", json!("-1.5"))]));
    assert!(v.issues.is_empty() && !v.has_errors(), "{v:?}");
}

#[test]
fn validate_bad_type_integer_boolean_float_and_bad_choice() {
    let db = Database::load(&root("fgd")).unwrap();
    // integer given text "abc" (the required negative).
    let v = db.validate("func_buyzone", &kv(&[("TeamNum", json!("abc"))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    assert_eq!(v.issues[0].kind, IssueKind::BadType);
    assert!(v.issues[0].message.contains("integer"), "{v:?}");

    // boolean given "maybe" on a boolean key.
    let v = db.validate("weapon_ak47", &kv(&[("CanBePickedUp", json!("maybe"))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    assert_eq!(v.issues[0].kind, IssueKind::BadType);
    assert!(v.issues[0].message.contains("0\" or \"1"), "{v:?}");

    // float.
    let v = db.validate("light_environment", &kv(&[("_fademindist", json!("not-a-float"))]));
    assert!(v.issues.iter().any(|i| i.kind == IssueKind::BadType && i.message.contains("float")), "{v:?}");
    // ... and a float text is fine.
    let v = db.validate("light_environment", &kv(&[("_fademindist", json!("-1.5"))]));
    assert!(v.issues.is_empty(), "{v:?}");

    // choices: a non-listed value is a BadChoice.
    let v = db.validate("light_environment", &kv(&[("clientSideEntity", json!("5"))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    assert_eq!(v.issues[0].kind, IssueKind::BadChoice);
    assert!(v.issues[0].message.contains("one of"), "{v:?}");
    // ... and a listed one is fine.
    let v = db.validate("light_environment", &kv(&[("clientSideEntity", json!("1"))]));
    assert!(v.issues.is_empty(), "{v:?}");
}

#[test]
fn validate_classname_is_forbidden_and_non_strings_render() {
    let db = Database::load(&root("fgd")).unwrap();
    // The required negative: `classname` is never settable.
    let v = db.validate("info_target", &kv(&[("classname", json!("info_target"))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    assert_eq!(v.issues[0].kind, IssueKind::Forbidden);
    assert!(v.issues[0].message.contains("classname"), "{v:?}");

    // Bools and numbers render to valid text.
    let v = db.validate(
        "weapon_ak47",
        &kv(&[("CanBePickedUp", json!(true)), ("spawnflags", json!(4))]),
    );
    assert!(v.issues.is_empty(), "{v:?}");
    // An integer key given a fractional number is a BadType.
    let v = db.validate("func_buyzone", &kv(&[("TeamNum", json!(4.5))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    assert_eq!(v.issues[0].kind, IssueKind::BadType);
    // A JSON array cannot render to text anywhere.
    let v = db.validate("info_target", &kv(&[("bogus", json!([1, 2]))]));
    assert_eq!(v.issues.len(), 1, "{v:?}");
    assert_eq!(v.issues[0].kind, IssueKind::UnknownKey); // unknown first
}

#[test]
fn classes_listing_filters_by_kind_and_name() {
    let db = Database::load(&root("fgd")).unwrap();
    let solid: Vec<&str> = db
        .classes(None, &[ClassKind::Solid], usize::MAX)
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(solid.contains(&"func_buyzone"), "{solid:?}");
    assert!(!solid.contains(&"info_target"), "{solid:?}");

    let base: Vec<&str> = db
        .classes(Some("WEAPON"), &[ClassKind::Base], usize::MAX)
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(base.contains(&"WeaponMini") && base.contains(&"WeaponCore"), "{base:?}");

    let limited = db.classes(Some(""), &[], 2);
    assert_eq!(limited.len(), 2);
}

// ---------------------------------------------------------------- required negative tests

#[test]
fn unterminated_string_is_a_parse_error_with_the_line() {
    let err = Database::load(&root("fgd/cases/bad_string")).unwrap_err();
    match err {
        FgdError::Parse { line, reason, .. } => {
            assert_eq!(line, 4, "the string starts on line 4 of the fixture");
            assert!(reason.contains("unterminated string"), "{reason}");
        }
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn unterminated_metadata_brace_is_a_parse_error_not_a_hang() {
    let err = Database::load(&root("fgd/cases/unterminated_brace")).unwrap_err();
    match err {
        FgdError::Parse { line, reason, .. } => {
            assert_eq!(line, 4, "the '{{ … }}' span opens on line 4 of the fixture");
            assert!(reason.contains("unterminated"), "{reason}");
        }
        other => panic!("expected Parse, got {other:?}"),
    }
}

/// Pathological (but lexable) input must terminate: a directive with no tokens after it, a key
/// with no `(type)`, a `[` body that never closes, an empty body and a bare `@include`. The
/// parser's per-loop progress guards turn a stalled loop into an error, so this test finishing is
/// itself the assertion that nothing spins.
#[test]
fn pathological_input_terminates_with_warnings_not_a_hang() {
    let db = Database::load(&root("fgd/cases/pathological"))
        .expect("pathological input must terminate without a fatal error");
    assert_eq!(db.files.len(), 2, "files: {:?}", db.files);
    assert!(db.class("extra_parses").is_some(), "the included file loaded");
    assert!(
        db.class("survivor").is_some(),
        "the class after a directive with nothing after it still loads"
    );
    let truncated = db.class("truncated").expect("an unclosed body still yields its class");
    assert!(
        truncated.keys.iter().any(|k| k.name == "lonely"),
        "keys before EOF: {:?}",
        truncated.keys
    );
    assert!(
        db.warnings.iter().any(|w| w.contains("without a (type)")),
        "the key without a type is warned about: {:#?}",
        db.warnings
    );
    assert!(
        db.warnings.iter().any(|w| w.contains("expected a helper")),
        "the empty directive is warned about: {:#?}",
        db.warnings
    );
    assert!(
        db.warnings.iter().any(|w| w.contains("@include without a quoted file name")),
        "the bare @include is warned about: {:#?}",
        db.warnings
    );
}

/// `@exclude Name` hides the definitions seen so far, and a later definition of the same name
/// brings the class back — the real csgo.fgd excludes 41 base classes and redefines most of them.
#[test]
fn exclude_hides_older_definitions_and_a_redefinition_revives_the_class() {
    let db = Database::load(&root("fgd/cases/exclude")).unwrap();
    // Never redefined: invisible, and not listed.
    assert!(db.class("dropped").is_none(), "an excluded class is invisible");
    assert!(
        !db.classes(None, &[ClassKind::Point], usize::MAX)
            .iter()
            .any(|c| c.name == "dropped")
    );
    // Excluded and then redefined: the new definition is the class, without a duplicate warning.
    let revived = db.class("revived").expect("the redefinition revives the class");
    assert_eq!(revived.description.as_deref(), Some("Redefined after the exclude."));
    assert!(revived.keys.iter().any(|k| k.name == "new_key"), "{:?}", revived.keys);
    assert!(!revived.keys.iter().any(|k| k.name == "old_key"), "{:?}", revived.keys);
    // The base excluded with the old definition is still resolved for the new one.
    assert!(revived.keys.iter().any(|k| k.name == "base_key"));
    assert!(db.warnings.is_empty(), "warnings: {:#?}", db.warnings);
}

#[test]
fn missing_include_is_a_warning_not_an_error() {
    let db = Database::load(&root("fgd/cases/missing_include"))
        .expect("a missing @include must not be fatal");
    assert_eq!(db.files.len(), 1, "files: {:?}", db.files);
    assert!(db.class("survivor").is_some(), "the rest of the file still loads");
    let w = db
        .warnings
        .iter()
        .find(|w| w.contains("does_not_exist.fgd"))
        .unwrap_or_else(|| panic!("warnings: {:#?}", db.warnings));
    assert!(w.contains("csgo.fgd:3:"), "{w}");
    assert!(w.contains("not found"), "{w}");
}

#[test]
fn include_cycle_is_an_error_naming_the_chain() {
    let err = Database::load(&root("fgd/cases/cycle")).unwrap_err();
    match &err {
        FgdError::IncludeCycle { chain } => {
            assert!(chain.contains("csgo.fgd") && chain.contains("cycle.fgd"), "{chain}");
            assert!(chain.split(" -> ").count() >= 3, "{chain}");
        }
        other => panic!("expected IncludeCycle, got {other:?}"),
    }
}

#[test]
fn root_without_csgo_fgd_is_unavailable() {
    let err = Database::load(&root("fgd/cases/missing")).unwrap_err();
    match &err {
        FgdError::Unavailable { path } => {
            assert!(path.ends_with("csgo/csgo.fgd"), "{path:?}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ---------------------------------------------------------------- load_for (cache)

fn hammer_state_with_fgd_root(fgd_root: &Path) -> HammerState {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let argv = vec![
        "t".to_string(),
        // hermetic: never touches the host's real podman/project tree.
        "--podman".into(),
        "/nonexistent/cs2mcp-test-podman".into(),
        "--vk-layer-dir".into(),
        "/nonexistent/cs2mcp-test-vklayer".into(),
        "--project-root".into(),
        "/nonexistent/cs2mcp-test-project".into(),
    ];
    let mut state = HammerState::from_config(&W::parse_from(argv).c).unwrap();
    state.fgd_root = fgd_root.to_path_buf();
    state
}

#[test]
fn load_for_caches_by_root_and_mtimes() {
    let state = hammer_state_with_fgd_root(&root("fgd"));
    let first = hammer_mcp::fgd::load_for(&state).unwrap();
    let second = hammer_mcp::fgd::load_for(&state).unwrap();
    assert!(Arc::ptr_eq(&first, &second), "the second load must be a cache hit");
    assert!(first.class("info_target").is_some());
    // A different root must not hit the cache.
    let state2 = hammer_state_with_fgd_root(&root("fgd/cases/badclass"));
    let other = hammer_mcp::fgd::load_for(&state2).unwrap();
    assert!(!Arc::ptr_eq(&first, &other));
    assert!(other.class("still_parses").is_some());
    // A missing root propagates the error.
    let state3 = hammer_state_with_fgd_root(&root("fgd/cases/missing"));
    assert!(matches!(
        hammer_mcp::fgd::load_for(&state3),
        Err(FgdError::Unavailable { .. })
    ));
}

// ---------------------------------------------------------------- trigger + spawnflags (WP-F)

/// The declared flag bits of a class's `spawnflags` key, sorted: the resolved flag list is in FGD
/// declaration order (the base class's bits first when a subclass adds its own), so a merge is
/// checked as a set.
fn def_bits(db: &Database, class: &str) -> Vec<u64> {
    let mut bits: Vec<u64> = db
        .spawnflags_def(class)
        .expect("the class declares spawnflags")
        .flags
        .iter()
        .map(|flag| flag.bit)
        .collect();
    bits.sort_unstable();
    bits
}

/// The bits a decoded view reports as on, in declaration order.
fn bits_on(view: &hammer_mcp::fgd::SpawnflagsView) -> Vec<u64> {
    view.flags
        .iter()
        .filter(|flag| flag.on)
        .map(|flag| flag.bit)
        .collect()
}

const TRIGGER_ONCE_BITS: [u64; 12] = [1, 2, 4, 8, 16, 32, 64, 512, 1024, 2048, 4096, 268_435_456];

/// `is_trigger_class`: a **`@SolidClass`** whose resolved base chain (the `prop_tag` walk) carries
/// `TriggerOnce`/`Trigger`, or a `tags( … Trigger … )` helper arg — case-insensitively. A
/// `@SolidClass` without either is not a trigger, and neither is a Point class, a class the
/// database lacks, or a base class carrying the marker itself (only a Solid class is a volume).
#[test]
fn trigger_classes_are_recognized_by_base_chain_or_tag() {
    let db = Database::load(&root("fgd")).unwrap();
    for class in ["trigger_once", "trigger_multiple", "TRIGGER_PUSH"] {
        assert!(db.is_trigger_class(class), "{class} must be a trigger");
    }
    // NEGATIVE (the kind gate): the `@BaseClass`es that CARRY the name/tag are not triggers —
    // neither can be placed as a brush entity, so the predicate refuses them.
    for class in ["TriggerOnce", "TRIGGERONCE", "Trigger", "trigger_missing"] {
        assert!(
            !db.is_trigger_class(class),
            "{class} must not be a trigger: a @BaseClass is not a brush volume"
        );
    }
    assert_eq!(db.class("TriggerOnce").unwrap().kind, ClassKind::Base);
    assert_eq!(db.class("Trigger").unwrap().kind, ClassKind::Base);
    assert_eq!(
        db.class("trigger_once").unwrap().bases,
        vec!["TriggerOnce".to_string()]
    );
    assert_eq!(
        db.class("trigger_multiple").unwrap().bases,
        vec!["Trigger".to_string()]
    );
    // The chain is what counts: both `@SolidClass`es only inherit it.
    assert!(
        db.class("trigger_once")
            .unwrap()
            .helpers
            .iter()
            .all(|(name, _)| name != "tags"),
        "the fixture spells `tags( Trigger )` on the base class only"
    );

    for class in [
        "func_buyzone",
        "func_wall_mini",
        "info_target",
        "light_environment",
    ] {
        assert!(!db.is_trigger_class(class), "{class} must not be a trigger");
    }
    // The required negative: `func_buyzone` is Solid but not a trigger (base `Targetname` only);
    // `func_wall_mini` is the other @SolidClass without Trigger; a Point class never is one.
    assert_eq!(db.class("func_buyzone").unwrap().kind, ClassKind::Solid);
    assert_eq!(
        db.class("func_buyzone").unwrap().resolved_bases,
        vec!["Targetname".to_string(), "GameEntity".to_string()]
    );
    assert_eq!(db.class("func_wall_mini").unwrap().kind, ClassKind::Solid);
    assert_eq!(db.class("info_target").unwrap().kind, ClassKind::Point);
    assert!(db.class("trigger_missing").is_none());
}

/// NEGATIVE (the `flags` type gate, review-rust.md B4): a class that declares
/// `spawnflags(integer)` has no flag list — `spawnflags_def` is `None`, so both directions answer
/// "the class declares no flags" instead of an empty flag list whose `unknown_bits` is the whole
/// value. Only the `flags` type is a flag list.
#[test]
fn spawnflags_def_requires_the_flags_type() {
    let db = Database::load(&root("fgd/cases/spawnflags_type")).unwrap();
    let def = db.class("func_int_spawnflags_mini").unwrap();
    assert_eq!(def.kind, ClassKind::Solid);
    let key = def
        .keys
        .iter()
        .find(|k| k.name == "spawnflags")
        .expect("the fixture declares spawnflags");
    assert_eq!(
        key.type_, "integer",
        "the fixture spells spawnflags(integer)"
    );
    assert!(
        db.spawnflags_def("func_int_spawnflags_mini").is_none(),
        "an integer spawnflags is not a flag list"
    );
    match db.decode_spawnflags("func_int_spawnflags_mini", "4097") {
        Err(SpawnflagsError::NoSpawnflags { class }) => {
            assert_eq!(class, "func_int_spawnflags_mini")
        }
        other => panic!("expected NoSpawnflags for an integer spawnflags, got {other:?}"),
    }
    assert_eq!(db.default_spawnflags("func_int_spawnflags_mini"), 0);
    match db.encode_spawnflags("func_int_spawnflags_mini", &[json!("Clients")]) {
        Err(UnknownSpawnflag { class, valid, .. }) => {
            assert_eq!(class, "func_int_spawnflags_mini");
            assert!(valid.is_empty(), "no label to offer: {valid:?}");
        }
        other => panic!("expected UnknownSpawnflag for an integer spawnflags, got {other:?}"),
    }
}

/// NEGATIVE (the kind gate, review-rust.md B4): `tags( Trigger )` on a `@PointClass`, and the
/// `@BaseClass` `TriggerOnce` itself, carry the marker but are not trigger classes — only a
/// `@SolidClass` can be a brush volume. The same tag on a `@SolidClass` IS a trigger.
#[test]
fn is_trigger_class_requires_a_solid_class() {
    let db = Database::load(&root("fgd/cases/spawnflags_type")).unwrap();
    assert!(
        db.is_trigger_class("trigger_tagged_solid_mini"),
        "a SolidClass tagged Trigger is a trigger"
    );
    assert_eq!(
        db.class("trigger_tagged_solid_mini").unwrap().kind,
        ClassKind::Solid
    );
    assert!(
        !db.is_trigger_class("trigger_point_mini"),
        "a PointClass with tags( Trigger ) is not a brush volume"
    );
    assert_eq!(
        db.class("trigger_point_mini").unwrap().kind,
        ClassKind::Point
    );
    assert!(
        !db.is_trigger_class("TriggerOnce"),
        "the @BaseClass TriggerOnce is not placeable"
    );
    assert_eq!(db.class("TriggerOnce").unwrap().kind, ClassKind::Base);
    assert!(!db.is_trigger_class("GameEntity"));
}

/// WP-S3: the Sound tag through the `base()` chain. `point_soundevent` carries `tags( Sound )`,
/// every `snd_event_*` subclass re-declares it (the real file does the same), and
/// `snd_soundscape` has **no** helper of its own: its tag comes from `env_soundscape` through
/// `base()` — the inheritance this test pins. `logic_auto`/`logic_timer` carry the real
/// `tags( Logic )`.
#[test]
fn sound_classes_are_recognized_by_the_tags_helper_through_the_chain() {
    let db = Database::load(&root("fgd")).unwrap();
    for class in [
        "ambient_generic",
        "point_soundevent",
        "snd_event_point",
        "snd_event_sphere",
        "snd_event_alignedbox",
        "env_soundscape",
        "snd_soundscape",
    ] {
        assert!(db.is_sound_class(class), "{class} carries tags( Sound )");
        assert!(db.has_tag(class, "Sound"), "{class}");
        // the tag is case-insensitive, like every other FGD name
        assert!(db.has_tag(class, "sound"), "{class}");
    }
    // The base-chain half: `snd_soundscape` re-declares nothing of its own.
    let subclass = db.class("snd_soundscape").expect("in the fixture");
    assert_eq!(subclass.resolved_bases[0], "env_soundscape");
    assert!(
        subclass
            .helpers
            .iter()
            .all(|(name, _)| name != "tags"),
        "the fixture's snd_soundscape has no tags() helper of its own: {:?}",
        subclass.helpers
    );
    assert!(
        db.has_tag("snd_soundscape", "Sound"),
        "the tag resolves through env_soundscape"
    );
    // ... and the inheritance really is the resolution: the subclass's chain is what it inherits.
    assert_eq!(
        db.class("snd_event_point").unwrap().resolved_bases,
        vec![
            "point_soundevent".to_string(),
            "Targetname".to_string(),
            "GameEntity".to_string(),
            "Parentname".to_string()
        ],
        "the chain the tag walk follows"
    );
    assert!(
        db.has_tag("logic_auto", "Logic") && db.has_tag("logic_timer", "Logic"),
        "the timing helpers carry the real tags( Logic )"
    );
}

/// WP-S3 NEGATIVE: a class without the tag is not a sound class, whether it carries another tag,
/// a `soundscape` key, or is not in the database at all. `prop_static` is the WP's named case (it
/// lives in the props fixture appended by `tests/common`'s `FgdTree`, not in this tree — an absent
/// class is a negative too).
#[test]
fn is_sound_class_is_false_without_the_tag() {
    let db = Database::load(&root("fgd")).unwrap();
    for class in [
        "prop_static",
        "info_target",
        "func_buyzone",
        "trigger_once",
        "trigger_soundscape",
        "no_such_class",
    ] {
        assert!(!db.is_sound_class(class), "{class} must not be a sound class");
    }
    // `trigger_soundscape` is the sharp one: it has a `soundscape` keyvalue, so a check keyed on
    // the key *name* would call it a sound class. The tag is the marker.
    let trigger = db.class("trigger_soundscape").expect("in the fixture");
    assert_eq!(trigger.kind, ClassKind::Solid);
    assert!(
        trigger.keys.iter().any(|key| key.name == "soundscape"),
        "the key that must NOT make it a sound class: {:?}",
        trigger.keys
    );
    assert!(db.sound_keys("trigger_soundscape").is_empty());
    // Its chain does carry another tag-carrying base (`Trigger`), so the walk really is
    // tag-driven, not "any base at all".
    assert_eq!(trigger.resolved_bases[0], "Trigger");
    assert!(!db.has_tag("trigger_soundscape", "Sound"));
}

/// WP-S3: `sound_keys` is the resolved keyvalues whose FGD type is `sound` — `soundName` for the
/// `snd_event_*` family (declared by `point_soundevent`), `soundevent` for the soundscapes,
/// `message` for the legacy `ambient_generic`. `env_soundscape.soundscape` is type `string` and is
/// therefore **not** listed; `info_target` declares none.
#[test]
fn sound_keys_lists_only_the_sound_typed_keyvalues() {
    let db = Database::load(&root("fgd")).unwrap();
    let names = |class: &str| -> Vec<String> {
        db.sound_keys(class)
            .iter()
            .map(|key| key.name.clone())
            .collect()
    };
    assert_eq!(names("point_soundevent"), vec!["soundName"]);
    assert_eq!(names("snd_event_point"), vec!["soundName"]);
    assert_eq!(names("snd_event_sphere"), vec!["soundName"]);
    assert_eq!(names("snd_event_alignedbox"), vec!["soundName"]);
    assert_eq!(names("env_soundscape"), vec!["soundevent"]);
    assert_eq!(names("snd_soundscape"), vec!["soundevent"]);
    assert_eq!(names("ambient_generic"), vec!["message"]);
    assert!(names("info_target").is_empty(), "no sound key");
    assert!(names("no_such_class").is_empty(), "not in the database");
    // The declared type is carried by the KeyDef the check selects on.
    for class in ["point_soundevent", "env_soundscape", "ambient_generic"] {
        for key in db.sound_keys(class) {
            assert_eq!(key.type_, "sound", "{}.{}", class, key.name);
        }
    }
    // `env_soundscape.soundscape` exists and is NOT one of them (type `string`: a legacy .vsc).
    let soundscape = db
        .class("env_soundscape")
        .unwrap()
        .keys
        .iter()
        .find(|key| key.name == "soundscape")
        .expect("soundscape key");
    assert_eq!(soundscape.type_, "string");
}

/// The fixture's `TriggerOnce` carries the real 12 flags with their real defaults, and its
/// `metadata { … }` helper body survived the parser (the WP's first open question). The
/// `{ … }` args are raw text, so all three entries are pinned here.
#[test]
fn trigger_once_carries_the_real_flags_and_the_metadata_body() {
    let db = Database::load(&root("fgd")).unwrap();
    let def = db
        .spawnflags_def("trigger_once")
        .expect("trigger_once spawnflags");
    assert_eq!(def.name, "spawnflags");
    assert_eq!(def.type_, "flags");
    assert_eq!(def.declared_in, "TriggerOnce", "declared by the base class");
    assert_eq!(def_bits(&db, "trigger_once"), TRIGGER_ONCE_BITS.to_vec());
    assert_eq!(def.flags.len(), 12);

    let clients = def.flags.iter().find(|flag| flag.bit == 1).expect("bit 1");
    assert_eq!(clients.label, "Clients");
    assert!(clients.default_on, "1: \"Clients\" : 1");
    let npcs = def.flags.iter().find(|flag| flag.bit == 2).expect("bit 2");
    assert_eq!(npcs.label, "NPCs");
    assert!(!npcs.default_on);
    let mass = def
        .flags
        .iter()
        .find(|flag| flag.bit == 4096)
        .expect("bit 4096");
    assert!(mass.default_on, "the object-mass flag defaults on");
    let hierarchy = def
        .flags
        .iter()
        .find(|flag| flag.bit == 268_435_456)
        .expect("bit 2^28");
    assert_eq!(hierarchy.label, "Touch objects in the trigger's hierarchy");
    assert!(!hierarchy.default_on);

    // `default_spawnflags` is the OR of the `default_on` bits: 1 | 4096 = 4097.
    assert_eq!(db.default_spawnflags("trigger_once"), 4097);
    assert_eq!(
        db.default_spawnflags("trigger_multiple"),
        4097,
        "inherited through Trigger"
    );
    assert_eq!(
        db.default_spawnflags("trigger_push"),
        4097,
        "its own 128/256 default off"
    );

    let base = db.class("TriggerOnce").expect("TriggerOnce");
    let (_, args) = base
        .helpers
        .iter()
        .find(|(name, _)| name == "metadata")
        .expect("the metadata helper is captured");
    for needle in [
        "auto_apply_material",
        "collision_group",
        "physics_only_model",
        "materials/tools/toolstrigger.vmat",
    ] {
        assert!(
            args.contains(needle),
            "the metadata body keeps {needle}: {args:?}"
        );
    }
    // The subclass does not repeat the helper; only the chain reaches it.
    assert!(
        db.class("trigger_once")
            .unwrap()
            .helpers
            .iter()
            .all(|(name, _)| name != "metadata"),
        "the fixture spells metadata on the base class only"
    );
}

/// A subclass's own `spawnflags` block merges with the base's (Hammer keeps the inherited flags);
/// the subclass's label/default wins for a bit both declare.
#[test]
fn subclass_spawnflags_merge_with_the_base_bits() {
    let db = Database::load(&root("fgd")).unwrap();
    // trigger_push declares 128/256 only; the resolved key still offers TriggerOnce's 12.
    let mut expected = TRIGGER_ONCE_BITS.to_vec();
    expected.extend([128, 256]);
    expected.sort_unstable();
    assert_eq!(def_bits(&db, "trigger_push"), expected);
    let def = db.spawnflags_def("trigger_push").unwrap();
    assert_eq!(def.flags.len(), 14);
    assert_eq!(
        def.declared_in, "trigger_push",
        "the child's key carries the merge"
    );
    let once = def
        .flags
        .iter()
        .find(|flag| flag.bit == 128)
        .expect("bit 128");
    assert_eq!(once.label, "Once Only");
    assert!(!once.default_on);
    let ladders = def
        .flags
        .iter()
        .find(|flag| flag.bit == 256)
        .expect("bit 256");
    assert_eq!(ladders.label, "Affects Ladders (Half-Life 2)");
    assert_eq!(
        def.flags.iter().find(|flag| flag.bit == 1).unwrap().label,
        "Clients",
        "the base's label survives the merge"
    );
    // A class that declares none of its own sees the inherited key untouched.
    assert_eq!(
        db.spawnflags_def("trigger_multiple").unwrap().declared_in,
        "TriggerOnce"
    );
}

/// `auto_apply_material` comes from the `metadata { }` header helper through the base chain, and
/// only from there: metadata without the key, or no metadata at all, is `None` (never invented).
#[test]
fn auto_apply_material_follows_the_metadata_helper_through_the_chain() {
    let db = Database::load(&root("fgd")).unwrap();
    for class in [
        "TriggerOnce",
        "trigger_once",
        "trigger_multiple",
        "trigger_push",
        "TRIGGER_PUSH",
    ] {
        assert_eq!(
            db.auto_apply_material(class).as_deref(),
            Some("materials/tools/toolstrigger.vmat"),
            "{class}"
        );
    }
    // A metadata block that does not carry the key (the csgo fixture's light_environment).
    assert!(db
        .class("light_environment")
        .unwrap()
        .helpers
        .iter()
        .any(|(name, _)| name == "metadata"));
    assert_eq!(db.auto_apply_material("light_environment"), None);
    // No metadata anywhere in the chain.
    for class in [
        "func_buyzone",
        "func_wall_mini",
        "info_target",
        "trigger_missing",
    ] {
        assert_eq!(db.auto_apply_material(class), None, "{class}");
    }
}

/// Decoding the stored text: every declared flag is reported with its state, `""` is 0, and bits
/// the class does not declare are counted as unknown instead of being dropped.
#[test]
fn decode_spawnflags_reports_every_declared_flag() {
    let db = Database::load(&root("fgd")).unwrap();
    let view = db.decode_spawnflags("trigger_once", "4097").unwrap();
    assert_eq!(view.value, 4097);
    assert_eq!(view.unknown_bits, 0);
    assert_eq!(view.flags.len(), 12);
    assert_eq!(bits_on(&view), vec![1, 4096]);
    assert_eq!(
        view.flags.iter().find(|flag| flag.bit == 1).unwrap().label,
        "Clients"
    );

    // `""` (and whitespace-only text) is 0, and a value with surrounding whitespace still parses.
    for text in ["", "   ", "\t\n"] {
        let view = db.decode_spawnflags("trigger_once", text).unwrap();
        assert_eq!(view.value, 0, "{text:?}");
        assert!(view.flags.iter().all(|flag| !flag.on), "{text:?}");
        assert_eq!(view.unknown_bits, 0, "{text:?}");
    }
    assert_eq!(
        db.decode_spawnflags("trigger_once", " 4097 \n")
            .unwrap()
            .value,
        4097
    );

    // An undeclared bit is reported, not silently dropped, and does not disturb the declared ones.
    let view = db.decode_spawnflags("trigger_once", "65536").unwrap();
    assert_eq!(view.value, 65536);
    assert_eq!(view.unknown_bits, 65536);
    assert!(view.flags.iter().all(|flag| !flag.on));
    // 128 is trigger_push's bit, unknown on trigger_once; 136 = 128 | 8 decodes bit 8 only.
    let view = db.decode_spawnflags("trigger_once", "136").unwrap();
    assert_eq!(view.unknown_bits, 128);
    assert_eq!(bits_on(&view), vec![8]);

    // trigger_push decodes its own merged bits.
    let view = db.decode_spawnflags("trigger_push", "128").unwrap();
    assert_eq!(view.flags.len(), 14);
    assert_eq!(bits_on(&view), vec![128]);
    assert_eq!(view.unknown_bits, 0);
}

/// NEGATIVE: `"abc"`, `"1.5"`, a negative, an overflow and other non-integer text are all a
/// `NotInteger` error naming the class and the offending text — never a 0 or a partial decode.
#[test]
fn decode_spawnflags_rejects_non_integer_text() {
    let db = Database::load(&root("fgd")).unwrap();
    for text in [
        "abc",
        "1.5",
        "-1",
        "-0",
        "0x10",
        "4097x",
        "+",
        "1 2",
        "18446744073709551616",
    ] {
        match db.decode_spawnflags("trigger_once", text) {
            Err(SpawnflagsError::NotInteger { class, text: got }) => {
                assert_eq!(class, "trigger_once");
                assert_eq!(got, text, "the offending text is reported verbatim");
            }
            other => panic!("{text:?}: expected NotInteger, got {other:?}"),
        }
    }
}

/// NEGATIVE: a class without `spawnflags` fails both directions — decode is an error (not 0) and
/// encode refuses any name — for a Solid and an unknown class alike. (`info_target` is not one of
/// these: it declares its own two flags, which the encode tests below use.)
#[test]
fn classes_without_spawnflags_decode_and_encode_are_errors() {
    let db = Database::load(&root("fgd")).unwrap();
    for class in ["func_buyzone", "func_wall_mini", "trigger_missing"] {
        assert!(
            db.spawnflags_def(class).is_none(),
            "{class} declares no spawnflags"
        );
        match db.decode_spawnflags(class, "0") {
            Err(SpawnflagsError::NoSpawnflags { class: got }) => assert_eq!(got, class),
            other => panic!("{class}: expected NoSpawnflags, got {other:?}"),
        }
        assert_eq!(
            db.default_spawnflags(class),
            0,
            "{class}: nothing defaults on"
        );
        match db.encode_spawnflags(class, &[json!("Clients")]) {
            Err(UnknownSpawnflag {
                class: got,
                name,
                valid,
            }) => {
                assert_eq!(got, class);
                assert_eq!(name, "Clients");
                assert!(
                    valid.is_empty(),
                    "{class}: no declared label to offer: {valid:?}"
                );
            }
            other => panic!("{class}: expected UnknownSpawnflag, got {other:?}"),
        }
        // Nothing named at all is 0 (an empty list encodes the value 0 for any class).
        assert_eq!(db.encode_spawnflags(class, &[]).unwrap(), 0);
    }
}

/// Encoding accepts a declared label case-insensitively or an integer equal to a declared bit,
/// including the bits a subclass merged in.
#[test]
fn encode_spawnflags_accepts_labels_and_declared_bits() {
    let db = Database::load(&root("fgd")).unwrap();
    assert_eq!(
        db.encode_spawnflags("trigger_once", &[json!("Clients")])
            .unwrap(),
        1
    );
    assert_eq!(
        db.encode_spawnflags("trigger_once", &[json!("clients"), json!("CLIENTS")])
            .unwrap(),
        1,
        "labels are case-insensitive and idempotent"
    );
    assert_eq!(
        db.encode_spawnflags("trigger_once", &[json!("Clients"), json!(4096)])
            .unwrap(),
        4097,
        "labels and declared bits mix"
    );
    assert_eq!(
        db.encode_spawnflags("trigger_once", &[json!("Only player ally NPCs")])
            .unwrap(),
        16
    );
    assert_eq!(
        db.encode_spawnflags("trigger_once", &[json!(268_435_456)])
            .unwrap(),
        268_435_456
    );
    assert_eq!(db.encode_spawnflags("trigger_once", &[]).unwrap(), 0);
    assert_eq!(
        db.encode_spawnflags("trigger_push", &[json!("Once Only")])
            .unwrap(),
        128
    );
    assert_eq!(
        db.encode_spawnflags("trigger_push", &[json!(256)]).unwrap(),
        256
    );
    assert_eq!(
        db.encode_spawnflags("trigger_push", &[json!("Clients"), json!("Once Only")])
            .unwrap(),
        129,
        "a base bit and a merged-in bit together"
    );
}

/// NEGATIVE: an unknown label, an integer that is not a declared bit, and a wrong-typed value are
/// all `UnknownSpawnflag` with the declared labels offered — and the first failure ends the
/// encode, so a later good value cannot smuggle a value out.
#[test]
fn encode_spawnflags_rejects_unknown_names_and_undeclared_bits() {
    let db = Database::load(&root("fgd")).unwrap();
    match db.encode_spawnflags("trigger_once", &[json!("Dogs")]) {
        Err(UnknownSpawnflag { class, name, valid }) => {
            assert_eq!(class, "trigger_once");
            assert_eq!(name, "Dogs");
            assert_eq!(
                valid.len(),
                12,
                "every declared label, in FGD order: {valid:?}"
            );
            assert!(valid.contains(&"Clients".to_string()), "{valid:?}");
            assert!(valid.contains(&"Physics debris".to_string()), "{valid:?}");
        }
        other => panic!("expected UnknownSpawnflag, got {other:?}"),
    }
    // 3 is not a declared bit of a 1/2 flag set (info_target), as a number or as text.
    for bad in [json!(3), json!("3")] {
        match db.encode_spawnflags("info_target", std::slice::from_ref(&bad)) {
            Err(UnknownSpawnflag { class, name, valid }) => {
                assert_eq!(class, "info_target");
                assert_eq!(name, "3");
                assert_eq!(
                    valid,
                    vec![
                        "Transmit to client (respect PVS)".to_string(),
                        "Always transmit to client (ignore PVS)".to_string()
                    ],
                    "{bad}"
                );
            }
            other => panic!("{bad}: expected UnknownSpawnflag, got {other:?}"),
        }
    }
    // An undeclared bit, and a label that belongs to another trigger class only.
    assert!(matches!(
        db.encode_spawnflags("trigger_once", &[json!(8192)]),
        Err(UnknownSpawnflag { .. })
    ));
    assert!(matches!(
        db.encode_spawnflags("trigger_once", &[json!("Once Only")]),
        Err(UnknownSpawnflag { .. })
    ));
    assert!(matches!(
        db.encode_spawnflags("info_target", &[json!("Clients")]),
        Err(UnknownSpawnflag { .. })
    ));
    // Wrong-typed values are refused, never a panic and never a silent 0.
    for bad in [
        json!(true),
        json!(false),
        json!(null),
        json!([1]),
        json!({"a": 1}),
        json!(1.5),
    ] {
        let err = db
            .encode_spawnflags("trigger_once", std::slice::from_ref(&bad))
            .unwrap_err();
        assert_eq!(err.class, "trigger_once", "{bad}");
        assert!(!err.name.is_empty(), "{bad}");
    }
    // The first bad value wins.
    let err = db
        .encode_spawnflags("trigger_once", &[json!("Clients"), json!("Dogs")])
        .unwrap_err();
    assert_eq!(err.name, "Dogs");
}

// ---------------------------------------------------------------- the real game tree

/// The Sound tag and the `sound`-typed keyvalues against the real game FGD tree (WP-S3 corpus
/// test): the tagged classes, the inheritance through `base()`, the `trigger_soundscape` negative
/// (a `soundscape` key but no `tags( Sound )`), and the tag-filter count
/// `list_entity_classes {tag:"Sound"}` reports — **29** measured here (plan-snd.md §3 step 0 says
/// 28: off by one). Run manually:
/// `cargo test -p hammer-mcp --test fgd -- --ignored real_game_sound_classes`
#[test]
#[ignore]
fn real_game_sound_classes() {
    let db = Database::load(&game_root()).expect("the real tree parses without fatal errors");
    for class in [
        "ambient_generic",
        "point_soundevent",
        "snd_event_point",
        "snd_event_sphere",
        "snd_event_alignedbox",
        "snd_event_orientedbox",
        "snd_event_cone",
        "snd_event_oriented_boxes",
        "snd_event_box_helper",
        "env_soundscape",
        "snd_soundscape",
        "env_soundscape_proxy",
        "snd_soundscape_proxy",
        "env_soundscape_triggerable",
        "snd_soundscape_triggerable",
    ] {
        assert!(db.is_sound_class(class), "{class} carries tags( Sound )");
    }
    // `trigger_soundscape` is `@SolidClass base(Trigger)` with a `soundscape` key and **no**
    // `tags( Sound )`: the tag is the marker, not the key (its soundscape is a legacy .vsc name).
    assert!(!db.is_sound_class("trigger_soundscape"));
    assert!(!db.is_sound_class("trigger_once"));
    assert!(!db.is_sound_class("prop_static"));
    assert!(!db.is_sound_class("info_target"));
    assert!(!db.is_sound_class("no_such_class"));
    // The tag filter's population: every class carrying the tag (through its chain), the number
    // `list_entity_classes {tag:"Sound"}` answers with the default kinds (point + solid).
    let tagged: Vec<String> = db
        .classes(None, &[], usize::MAX)
        .iter()
        .filter(|class| db.has_tag(&class.name, "Sound"))
        .map(|class| class.name.clone())
        .collect();
    let placed: Vec<&String> = db
        .classes(None, &[ClassKind::Point, ClassKind::Solid], usize::MAX)
        .iter()
        .filter(|class| db.has_tag(&class.name, "Sound"))
        .map(|class| &class.name)
        .collect();
    println!(
        "real FGD: {} classes tagged Sound ({} of them point/solid): {tagged:?}",
        tagged.len(),
        placed.len()
    );
    // MEASURED (2026-10-16, this install): 29 — plan-snd.md §3 step 0 says 28, which is off by
    // one (30 `tags( Sound )` declarations in the raw files, one of which does not survive into
    // the resolved class list). The acceptance must expect 29.
    assert_eq!(
        placed.len(),
        29,
        "the measured population for list_entity_classes {{tag:\"Sound\"}}: {placed:?}"
    );
    println!("tag-filter population (point+solid): {placed:?}");

    // `sound_keys` is the `sound`-typed key set: `env_soundscape.soundscape` (a legacy .vsc name,
    // type `string`) is NOT one, `soundevent` is.
    let names = |class: &str| -> Vec<String> {
        db.sound_keys(class)
            .iter()
            .map(|key| key.name.clone())
            .collect()
    };
    assert_eq!(names("env_soundscape"), vec!["soundevent"]);
    assert_eq!(names("snd_soundscape"), vec!["soundevent"]);
    assert_eq!(names("point_soundevent"), vec!["soundName"]);
    assert_eq!(names("snd_event_point"), vec!["soundName"]);
    assert_eq!(names("snd_event_sphere"), vec!["soundName"]);
    assert_eq!(names("ambient_generic"), vec!["message"]);
    assert_eq!(
        names("trigger_soundscape"),
        Vec::<String>::new(),
        "its `soundscape` key is a target_destination, not a soundevent"
    );
    assert!(names("info_target").is_empty());
    // The `sound` type must not be mistaken for another type anywhere in the corpus.
    for class in db.classes(None, &[], usize::MAX) {
        for key in &class.keys {
            if matches!(key.name.as_str(), "soundName" | "soundevent") {
                assert_eq!(
                    key.type_, "sound",
                    "{}.{} must be the FGD `sound` type",
                    class.name, key.name
                );
            }
        }
    }
}

/// The project root's real game tree `<project root>/workshop/run/app/game`. Found by walking
/// up from the crate directory (a worktree is nested several levels deeper than the main
/// checkout), and never optional: a missing game tree fails the test loudly.
fn game_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("workshop/run/app/game");
        if candidate.join("csgo/csgo.fgd").is_file() {
            return std::fs::canonicalize(&candidate).expect("canonicalize the game tree");
        }
        if !dir.pop() {
            panic!(
                "no project root with workshop/run/app/game above {}",
                env!("CARGO_MANIFEST_DIR")
            );
        }
    }
}

/// Parses the real game FGD tree under the project root. Run manually:
/// `cargo test -p hammer-mcp --test fgd -- --ignored --nocapture real_game_fgd_parses`
#[test]
#[ignore]
fn real_game_fgd_parses() {
    let db = Database::load(&game_root()).expect("the real tree parses without fatal errors");
    let point: Vec<&hammer_mcp::fgd::Class> = db.classes(None, &[ClassKind::Point], usize::MAX);
    // Report first (the numbers are part of the WP-F hand-off), then assert.
    println!(
        "real FGD: {} classes ({} point), {} files, {} warnings",
        db.classes(None, &[], usize::MAX).len(),
        point.len(),
        db.files.len(),
        db.warnings.len()
    );
    for f in &db.files {
        println!("file: {}", f.display());
    }
    for w in db.warnings.iter().take(10) {
        println!("warning: {w}");
    }
    assert!(point.len() >= 290, "expected >= 290 point classes, got {}", point.len());
    // 295 distinct point classes survive (the 7 loaded files declare 297 `@PointClass` plus 30
    // filter/path/keyframe/cable point-entity directives; duplicates and csgo.fgd's 41 `@exclude`
    // entries remove the rest). The brief's ">= 300" counts declarations, not the deduplicated,
    // `@exclude`d class list.
    for name in [
        "info_target",
        "func_buyzone",
        "light_omni2",
        "prop_dynamic",
        // `@exclude`d in csgo.fgd and then redefined there: visible again.
        "env_sky",
    ] {
        assert!(
            db.class(name).is_some(),
            "{name} must exist in the real tree"
        );
    }
    // `@exclude env_sprite` with no redefinition anywhere: hidden from the class list.
    assert!(
        db.class("env_sprite").is_none(),
        "an @exclude without a redefinition stays hidden"
    );
    assert_eq!(db.class("func_buyzone").unwrap().kind, ClassKind::Solid);
    // Acceptance from the plan §4.1: info_target-like class with targetname/parentname/flags.
    let it = db.class("info_target").unwrap();
    for key in ["targetname", "parentname", "spawnflags"] {
        assert!(it.keys.iter().any(|k| k.name == key), "{key} on info_target");
    }
    let sf = it.keys.iter().find(|k| k.name == "spawnflags").unwrap();
    assert_eq!(sf.flags.len(), 2, "info_target spawnflags: {:?}", sf.flags);
    // The real tree must not be riddled with parse failures.
    assert!(
        db.warnings.len() < 50,
        "{} warnings (first 10 above); the parser dropped classes it should have kept",
        db.warnings.len()
    );
}
/// The trigger helpers against the real game FGD tree (WP-F corpus test): the real `TriggerOnce`
/// flags, the subclass merge, the metadata material and the `func_buyzone` negative. Run manually:
/// `cargo test -p hammer-mcp --test fgd -- --ignored real_game_trigger_helpers`
#[test]
#[ignore]
fn real_game_trigger_helpers() {
    let db = Database::load(&game_root()).expect("the real tree parses without fatal errors");
    for class in [
        "trigger_once",
        "trigger_multiple",
        "trigger_push",
        "trigger_hurt",
        "trigger_teleport",
        "trigger_look",
        "trigger_gravity",
    ] {
        assert!(db.is_trigger_class(class), "{class} must be a trigger");
    }
    // The `@BaseClass`es that carry the marker themselves are NOT triggers (not Solid classes:
    // they cannot be placed as a brush entity) — the kind gate of review-rust.md B4.
    assert!(!db.is_trigger_class("TriggerOnce"));
    assert!(!db.is_trigger_class("Trigger"));
    assert_eq!(db.class("TriggerOnce").unwrap().kind, ClassKind::Base);
    // `func_buyzone` is the plan's Solid-but-not-a-trigger case; info_target is a Point class.
    assert!(!db.is_trigger_class("func_buyzone"));
    assert!(!db.is_trigger_class("info_target"));
    assert_eq!(db.class("func_buyzone").unwrap().kind, ClassKind::Solid);

    let def = db
        .spawnflags_def("trigger_once")
        .expect("trigger_once declares the inherited spawnflags");
    assert_eq!(
        def.flags.len(),
        12,
        "TriggerOnce's 12 flags: {:?}",
        def.flags
    );
    assert_eq!(def_bits(&db, "trigger_once"), TRIGGER_ONCE_BITS.to_vec());
    assert_eq!(db.default_spawnflags("trigger_once"), 4097);
    // trigger_push/trigger_look redeclare spawnflags with 128/256 only; the merge keeps the base's.
    let push = def_bits(&db, "trigger_push");
    for bit in [1, 128, 256, 268_435_456] {
        assert!(push.contains(&bit), "trigger_push has bit {bit}: {push:?}");
    }
    assert!(
        push.contains(&128) && push.contains(&1),
        "the merge is what makes both Hammer flags visible: {push:?}"
    );
    assert_eq!(
        db.auto_apply_material("trigger_once").as_deref(),
        Some("materials/tools/toolstrigger.vmat")
    );
    assert_eq!(db.auto_apply_material("func_buyzone"), None);
    let view = db.decode_spawnflags("trigger_once", "4097").unwrap();
    assert_eq!(view.unknown_bits, 0);
    assert_eq!(bits_on(&view), vec![1, 4096]);
    assert_eq!(
        db.encode_spawnflags("trigger_push", &[json!("Once Only")])
            .unwrap(),
        128
    );
    println!(
        "real FGD: trigger_once {} flags, defaults {}, push {} flags; auto_apply_material {:?}",
        def.flags.len(),
        db.default_spawnflags("trigger_once"),
        push.len(),
        db.auto_apply_material("trigger_once")
    );
}
