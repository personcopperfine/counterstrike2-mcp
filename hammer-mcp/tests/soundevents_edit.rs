//! `edit_soundevents` tests (WP-S2): the comment-preserving splice of the addon's
//! `soundevents_addon.vsndevts`, against the committed fixtures (`fixtures/soundevents/`) in a
//! temp project tree. No bridge, no container, no game.
//!
//! Since FIX-S2 the production path builds the REAL soundevent index (`soundevents::index_for`,
//! WP-S1) lazily, at the first base/children/keys check. Tests whose calls reach those checks
//! therefore run on a [`common::SoundTree`] fixture tree (a synthetic csgo+core VPK pair plus
//! the addon's compiled `_c` — `Tree::indexed`), whose index builds; tests refused before the
//! checks (argument negatives, path refusals, parse errors, an exists/remove refusal) stay on a
//! bare tree and must never build the index. `an_unbuildable_index_refuses_the_edit` pins the
//! policy on a bare tree: `validate:true` refuses with `VpkMissing` and writes nothing,
//! `validate:false` proceeds with a warning that no reference check ran. The seam tests call
//! [`hammer_mcp::tools::soundevents_edit::edit_soundevents_with`] directly: one injects a
//! resolver that knows a base the file does not, one injects a broken splicer to pin the
//! `InternalError`-refuses behaviour of the round-trip check.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::mcp::{McpServer, ServerIdentity};
use cs2_mcp_common::tools::{Budget, CallCtx, CallFailure, Registry, ToolError};
use hammer_mcp::soundevents::SoundEventIndex;
use hammer_mcp::soundevents::text::{SpliceError, SpliceOp, Spliced};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::soundevents_edit::{
    edit_soundevents_with, BaseResolver, EditSeams, ResolverSource, TEMPLATE_HEADER,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod common;

use common::SoundTree;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp project tree (`workshop/run/app`), the addon at `content/csgo_addons/mcp_test`.
/// `Tree::new` is bare (no game VPKs: the soundevent index cannot build); `Tree::indexed` wraps
/// WP-S1's `SoundTree` fixture (a synthetic csgo+core VPK pair and the addon's compiled `_c`),
/// whose index builds — the tree every test reaching the base/children/keys checks needs.
struct Tree {
    root: PathBuf,
    app: PathBuf,
    /// The `SoundTree` behind an indexed tree. Its own `Drop` removes the same root (silently
    /// ignoring the second removal).
    _sound: Option<SoundTree>,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "snd-edit-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        let app = root.join("workshop/run/app");
        Tree { root, app, _sound: None }
    }

    /// A tree whose soundevent index builds (FIX-S2): the `SoundTree` fixture's synthetic VPK
    /// pair and addon `_c`, with the addon's source text at the same path `install` overwrites.
    fn indexed(tag: &str) -> Tree {
        let sound = SoundTree::new(tag);
        let root = sound.root.clone();
        let app = sound.app.clone();
        Tree { root, app, _sound: Some(sound) }
    }

    /// Delete the addon's `.vsndevts` source (the `createFile` case on an indexed tree: the
    /// file must be missing while the index still builds from the `_c` and the VPKs).
    fn remove_addon_source(&self) {
        std::fs::remove_file(self.addon().join(DEFAULT_REL))
            .expect("remove the addon source text");
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    /// Install a fixture from `fixtures/soundevents/` as the addon's
    /// `soundevents/soundevents_addon.vsndevts` (or another `rel`).
    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        let target = self.addon().join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("install dir");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/soundevents")
            .join(fixture);
        std::fs::copy(&source, &target).expect("install fixture");
        target
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let target = self.addon().join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("write dir");
        std::fs::write(&target, text).expect("write file");
        target
    }

    /// The bytes of the addon file at `rel`.
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.addon().join(rel)).expect("read back")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(tree: &Tree) -> cs2_mcp_common::config::CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: cs2_mcp_common::config::CommonConfig,
    }
    let args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--tool-budget-ms".into(),
        "45000".into(),
        "--project-root".into(),
        tree.root.display().to_string(),
        "--path-map".into(),
        format!("z:\\app={}", tree.app.display()),
    ];
    let given: Vec<&str> = args.iter().map(String::as_str).collect();
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

fn state_for(tree: &Tree) -> HammerState {
    HammerState::from_config(&config(tree)).expect("state")
}

/// A server with exactly the WP-S2 tools registered, plus a handle on the same state (the Arc
/// fields — the soundevent index cache — are shared, so seeding one seeds the server's).
fn server_for(tree: &Tree) -> (McpServer<HammerState>, HammerState) {
    let cfg = config(tree);
    let state = HammerState::from_config(&cfg).expect("state");
    let mut registry = Registry::new();
    hammer_mcp::tools::soundevents_edit::register(&mut registry);
    let server = McpServer::new(
        registry,
        state.clone(),
        ServerIdentity { name: "hammer-mcp", version: "test", instructions: String::new() },
        cfg.tool_budget_ms,
        cfg.deadline_ms,
    );
    (server, state)
}

async fn call(
    server: &McpServer<HammerState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

/// Direct edit call with injected seams (no registry).
async fn edit_with(
    seams: &EditSeams,
    state: HammerState,
    args: Value,
) -> Result<Value, ToolError> {
    let ctx = CallCtx { budget: Budget::new(45_000, 5_000) };
    let out = edit_soundevents_with(seams, state, ctx, args).await?;
    Ok(out.json)
}

fn default_seams() -> EditSeams {
    EditSeams::default()
}

const DEFAULT_REL: &str = "soundevents/soundevents_addon.vsndevts";

fn set_op(name: &str, properties: Value) -> Value {
    json!({"op": "set", "name": name, "properties": properties})
}

/// Every line whose only content is a comment (`// …` after optional whitespace), in order.
fn comment_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.trim_start().starts_with("//"))
        .collect()
}

// -------------------------------------------------------------------------------------------
// registration
// -------------------------------------------------------------------------------------------

#[test]
fn register_exposes_the_two_tools_with_writer_policies() {
    let mut registry = Registry::new();
    hammer_mcp::tools::soundevents_edit::register(&mut registry);
    assert_eq!(
        registry.names(),
        hammer_mcp::tools::soundevents_edit::SOUND_EVENTS_EDIT_TOOL_ORDER.to_vec()
    );
    for name in ["edit_soundevents", "compile_soundevents"] {
        let spec = registry.spec(name).unwrap_or_else(|| panic!("{name} registered"));
        assert_eq!(spec.mutability, cs2_mcp_common::tools::Mutability::Writer, "{name}");
        assert!(spec.bridge_methods.is_empty(), "{name} needs no bridge methods");
    }
}

// -------------------------------------------------------------------------------------------
// positive: the ops round-trip
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn add_replace_merge_remove_rename_round_trip() {
    // the set ops reach the unknownKeys check, so the index must build (FIX-S2)
    let tree = Tree::indexed("roundtrip");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);

    // add
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.test.point", json!({
            "type": "csgo_mega",
            "volume": 0.9,
            "vsnd_files_track_01": "sounds/ambient/dust2/cablestress_01.vsnd",
        }))]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("created"));
    assert_eq!(out["written"], json!(true));
    assert_eq!(out["commentsDropped"], json!([]));
    let text = tree.read(DEFAULT_REL);
    let parsed = cs2_kv3::parse(&text).expect("the written file parses");
    assert_eq!(out["events"], json!(parsed.root.len()));
    let added = parsed.root.get("mcp.test.point").and_then(|v| v.as_object()).expect("added");
    assert!(
        added.get("volume").is_some(),
        "the numeric volume survives the JSON -> KV3 conversion"
    );
    assert_eq!(
        added.get("vsnd_files_track_01").and_then(cs2_kv3::Value::as_str),
        Some("sounds/ambient/dust2/cablestress_01.vsnd")
    );

    // merge keeps the untouched keys and overrides the given ones
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.test.point", json!({"volume": 0.5})), ]}),
    )
    .await;
    assert!(out.is_err(), "a set on an existing name needs overwrite/merge");

    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [json!({"op": "set", "name": "mcp.test.point", "merge": true,
            "properties": {"volume": 0.5}})]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("merged"));
    let parsed = cs2_kv3::parse(&tree.read(DEFAULT_REL)).unwrap();
    let merged = parsed.root.get("mcp.test.point").and_then(|v| v.as_object()).unwrap();
    assert_eq!(
        merged.get("vsnd_files_track_01").and_then(cs2_kv3::Value::as_str),
        Some("sounds/ambient/dust2/cablestress_01.vsnd"),
        "merge keeps the existing keys"
    );

    // replace drops what the caller did not re-state
    let out = call(
        &server,
        "edit_soundevents",
        json!({"overwrite": true, "ops": [json!({"op": "set", "name": "mcp.test.point",
            "properties": {"type": "csgo_mega", "volume": 0.1}})]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("replaced"));
    let parsed = cs2_kv3::parse(&tree.read(DEFAULT_REL)).unwrap();
    let replaced = parsed.root.get("mcp.test.point").and_then(|v| v.as_object()).unwrap();
    assert!(replaced.get("vsnd_files_track_01").is_none(), "replaced, not merged");

    // rename rewrites the key and the base references
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [json!({"op": "rename", "name": "alpha.event", "to": "alpha.renamed"})]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("renamed"));
    assert_eq!(out["referencesUpdated"], json!(1), "beta.event's base followed");
    let text = tree.read(DEFAULT_REL);
    assert!(text.contains("\t\"alpha.renamed\" = "), "the key token was rewritten");
    assert!(text.contains("base = \"alpha.renamed\""));
    assert!(!text.contains("\"alpha.event\""));

    // remove
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [{"op": "remove", "name": "mcp.test.point"}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("removed"));
    let parsed = cs2_kv3::parse(&tree.read(DEFAULT_REL)).unwrap();
    assert!(parsed.root.get("mcp.test.point").is_none());
}

#[tokio::test]
async fn comments_outside_the_touched_event_survive_byte_for_byte() {
    // the merge reaches the unknownKeys check, so the index must build (FIX-S2)
    let tree = Tree::indexed("comments");
    let before = std::fs::read_to_string(tree.install("soundevents_addon.vsndevts", DEFAULT_REL))
        .expect("fixture");
    let (server, _state) = server_for(&tree);

    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [json!({"op": "set", "name": "ambient_example.indoors.vent", "merge": true, "properties": {
            "base": "amb.looping.atXYZ.base",
            "volume": 0.9,
            "position": [1136.0, 1392.0, 64.0],
            "vsnd_files_track_01": "sounds/vent_01.vsnd",
        }})]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("merged"));
    assert_eq!(out["commentsDropped"], serde_json::json!([]), "the vent event carries no comments");

    let after = tree.read(DEFAULT_REL);
    // the header line is unchanged ...
    assert_eq!(after.lines().next(), before.lines().next(), "header line");
    // ... every comment line survives byte-for-byte, in order ...
    assert_eq!(comment_lines(&before), comment_lines(&after), "comment lines");
    // ... CRLF is kept everywhere ...
    let bare_lf = after.matches('\n').count() - after.matches("\r\n").count();
    assert_eq!(bare_lf, 0, "no bare LF: every newline is CRLF");
    // ... and the untouched event bodies are byte-identical too
    for name in ["ambient_example.outdoors", "amb.base", "amb.soundscapeParent.base"] {
        let grab = |text: &str| {
            let start = text.find(&format!("\"{name}\"")).expect(name);
            text[start..].split("\r\n\t\"").next().unwrap().to_string()
        };
        assert_eq!(grab(&before), grab(&after), "event {name} is untouched");
    }
}

#[tokio::test]
async fn create_file_writes_the_exact_template_header() {
    // an indexed tree whose addon source text is gone: the file is missing (createFile's case)
    // while the index still builds from the fixture's compiled _c and VPKs (FIX-S2)
    let tree = Tree::indexed("createfile");
    tree.remove_addon_source();
    let (server, _state) = server_for(&tree);
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.new.event", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventsFileNotFound");
    assert_eq!(err.data["hint"], json!("createFile:true"));

    let out = call(
        &server,
        "edit_soundevents",
        json!({"createFile": true, "ops": [set_op("mcp.new.event", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap();
    assert_eq!(out["created"], json!(true));
    let text = tree.read(DEFAULT_REL);
    assert_eq!(
        text.lines().next(),
        Some(TEMPLATE_HEADER),
        "the template's exact header line, verbatim"
    );
    assert!(cs2_kv3::parse(&text).is_ok());
    assert!(text.contains("\t\"mcp.new.event\" = \r\n\t{\r\n\t\ttype = \"csgo_mega\"\r\n\t}"));
}

#[tokio::test]
async fn dry_run_writes_nothing_and_backup_is_reported() {
    // the set op reaches the unknownKeys check, so the index must build (FIX-S2)
    let tree = Tree::indexed("dryrun");
    let path = tree.install("commented_events.vsndevts", DEFAULT_REL);
    let before = std::fs::read(&path).expect("read");
    let (server, _state) = server_for(&tree);

    let out = call(
        &server,
        "edit_soundevents",
        json!({"dryRun": true, "ops": [json!({"op": "set", "name": "beta.event", "merge": true, "properties": {"type": "csgo_mega"}})]}),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(false));
    assert_eq!(out["dryRun"], json!(true));
    assert_eq!(out["backup"], Value::Null);
    assert_eq!(std::fs::read(&path).expect("read"), before, "dryRun wrote nothing");
    let siblings: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!siblings.iter().any(|n| n.contains(".tmp-") || n.contains(".bak-")));

    // a real write of an existing file reports the backup beside it
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [json!({"op": "set", "name": "beta.event", "merge": true, "properties": {"type": "csgo_mega"}})]}),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(true));
    let backup = out["backup"].as_str().expect("backup path");
    assert!(Path::new(backup).is_file(), "the backup exists: {backup}");
    assert_eq!(
        std::fs::read(backup).unwrap(),
        before,
        "the backup holds the previous content"
    );
}

#[tokio::test]
async fn the_index_invalidation_hook_runs_after_a_write_only() {
    let tree = Tree::new("invalidate");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, state) = server_for(&tree);
    *state.soundevent_index.lock().unwrap() = Some(Arc::new(SoundEventIndex::default()));

    // a refusal does not touch the cached index
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.bad", json!({"base": "no.such.base"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventBaseNotFound");
    assert!(state.soundevent_index.lock().unwrap().is_some(), "no write, no invalidation");

    // a write drops it
    call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.new.event", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap();
    assert!(state.soundevent_index.lock().unwrap().is_none(), "the write invalidated the index");
}

// -------------------------------------------------------------------------------------------
// the real index (FIX-S2): the unbuildable policy, the corpus-backed warnings
// -------------------------------------------------------------------------------------------

/// The policy FIX-S2 keeps: on a tree without game VPKs the index cannot build — with
/// `validate:true` (the default) the edit is refused with that error and NOTHING is written;
/// with `validate:false` the edit proceeds with a warning that no reference check ran.
#[tokio::test]
async fn an_unbuildable_index_refuses_the_edit() {
    let tree = Tree::new("no-vpk");
    let path = tree.install("commented_events.vsndevts", DEFAULT_REL);
    let before = std::fs::read(&path).unwrap();
    let (server, _state) = server_for(&tree);

    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.test", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "VpkMissing");
    assert!(err.retry_safe);
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");

    // validate:false proceeds and warns
    let out = call(
        &server,
        "edit_soundevents",
        json!({"validate": false, "ops": [set_op("mcp.test", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(true));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| w["type"] == json!("indexUnavailable")
            && w["message"].as_str().unwrap_or_default().contains("no reference check ran")),
        "the no-reference-check warning: {warnings:?}"
    );
}

/// A property key the fixture corpus has never seen warns (unknownKeys) now that `known_keys`
/// comes from the real index; a key the corpus carries does not.
#[tokio::test]
async fn unknown_property_keys_warn_against_the_real_corpus() {
    let tree = Tree::indexed("real-keys");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);

    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.test", json!({"type": "csgo_mega", "not_a_real_key": 1}))]}),
    )
    .await
    .unwrap();
    let warnings = out["warnings"].as_array().expect("warnings");
    let unknown = warnings
        .iter()
        .find(|w| w["type"] == json!("unknownKeys"))
        .expect("unknownKeys warns now that known_keys is real");
    assert_eq!(unknown["entries"], json!([{"name": "mcp.test", "key": "not_a_real_key"}]));

    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.ok", json!({"type": "csgo_mega", "volume": 0.4}))]}),
    )
    .await
    .unwrap();
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().all(|w| w["type"] != json!("unknownKeys")),
        "a corpus key does not warn: {warnings:?}"
    );
}

/// An index with 0 events (the empty corpus) skips the key check — no flood of warnings for
/// keys the empty corpus has never seen. The cache is seeded, so this runs on a bare tree.
#[tokio::test]
async fn an_empty_corpus_skips_the_key_check() {
    let tree = Tree::new("empty-corpus");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, state) = server_for(&tree);
    *state.soundevent_index.lock().unwrap() = Some(Arc::new(SoundEventIndex::default()));

    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.test", json!({"type": "csgo_mega", "not_a_real_key": 1}))]}),
    )
    .await
    .unwrap();
    assert_eq!(out["written"], json!(true));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().all(|w| w["type"] != json!("unknownKeys")),
        "an empty corpus skips the key check: {warnings:?}"
    );
}

/// The base/children checks through the production path with the real index (the SoundTree
/// fixture's VPK pair): a base the file does not hold but the game VPKs do resolves, an unknown
/// child warns, and an unknown base is refused WITH the index's suggestions.
#[tokio::test]
async fn base_and_children_checks_run_through_the_real_index() {
    let tree = Tree::indexed("prod-index");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);

    // a base only the index knows (the fixture csgo VPK) resolves
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.test", json!({"base": "Weapon_AK47.Single"}))]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("created"));

    // a child no indexed source knows warns (known:false), never refuses
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.child", json!({"type": "csgo_mega", "soundevent_01": "no.such.child"}))]}),
    )
    .await
    .unwrap();
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| w["type"] == json!("unknownChild")
            && w["child"] == json!("no.such.child")),
        "the unknownChild warning: {warnings:?}"
    );

    // an unknown base is refused with the index's own suggestion ranking
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.bad", json!({"base": "Weapon_ak47.singl"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventBaseNotFound");
    assert_eq!(err.data["didYouMean"], json!(["Weapon_AK47.Single"]));
}

// -------------------------------------------------------------------------------------------
// negative tests (the WP list)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_comment_inside_the_edited_event_is_reported_not_preserved() {
    // the merge reaches the unknownKeys check, so the index must build (FIX-S2)
    let tree = Tree::indexed("comment-drop");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);

    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [json!({"op": "set", "name": "beta.event", "merge": true, "properties": {"volume": 0.3}})]}),
    )
    .await
    .unwrap();
    assert_eq!(
        out["commentsDropped"],
        json!([{"name": "beta.event", "lines": 1}]),
        "the `// a comment INSIDE beta.event` line"
    );
    // the comments OUTSIDE the event survive (between events, before the close)
    let after = tree.read(DEFAULT_REL);
    assert!(after.contains("// a comment BETWEEN events (before alpha)"));
    assert!(after.contains("spanning two lines, between events"));
    assert!(after.contains("// a trailing comment after the last event"));
}

#[tokio::test]
async fn unknown_base_refuses_and_writes_nothing() {
    // the base check consults the real index, so the index must build (FIX-S2)
    let tree = Tree::indexed("base");
    let path = tree.install("commented_events.vsndevts", DEFAULT_REL);
    let before = std::fs::read(&path).unwrap();
    let (server, _state) = server_for(&tree);

    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.bad", json!({"base": "amb.nope"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventBaseNotFound");
    assert_eq!(err.data["name"], json!("mcp.bad"));
    assert_eq!(err.data["base"], json!("amb.nope"));
    assert!(err.retry_safe);
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");

    // the edited file's own names resolve (root.get is consulted before the index)
    let out = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("gamma.event", json!({"base": "alpha.event"}))]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("created"));
}

#[tokio::test]
async fn an_event_with_neither_type_nor_base_is_refused() {
    // the set op reaches the unknownKeys check, so the index must build (FIX-S2)
    let tree = Tree::indexed("no-type");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);

    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.bad", json!({"volume": 0.5}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventInvalid");
    assert_eq!(err.data["name"], json!("mcp.bad"));
    assert_eq!(err.data["problems"], json!(["neither type nor base"]));
}

#[tokio::test]
async fn set_on_an_existing_name_without_overwrite_or_merge_refuses() {
    let tree = Tree::new("exists");
    let path = tree.install("commented_events.vsndevts", DEFAULT_REL);
    let before = std::fs::read(&path).unwrap();
    let (server, _state) = server_for(&tree);

    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("alpha.event", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventExists");
    assert_eq!(err.data["name"], json!("alpha.event"));
    assert!(err.data["file"].as_str().expect("file").contains("soundevents_addon.vsndevts"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn bad_names_are_invalid_arguments() {
    let tree = Tree::new("names");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);
    let bads: Vec<String> =
        vec!["has space".into(), "has/slash".into(), "a".repeat(128), String::new()];
    for bad in &bads {
        let err = call(
            &server,
            "edit_soundevents",
            json!({"ops": [set_op(bad.as_str(), json!({"type": "csgo_mega"}))]}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{bad:?}");
        assert_eq!(err.code, Some(-32602));
    }
}

#[tokio::test]
async fn properties_must_be_an_object_and_convert() {
    let tree = Tree::new("props");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);

    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [{"op": "set", "name": "mcp.bad", "properties": 5}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");

    // a value from_json cannot convert: a blob with a broken hex payload names the key
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("mcp.bad", json!({"volume": {"$blob": "nothex"}}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");
    assert!(err.message.contains("volume"), "the error names the key: {}", err.message);
}

#[tokio::test]
async fn remove_of_a_referenced_event_refuses_force_removes_and_warns() {
    let tree = Tree::new("remove-ref");
    let path = tree.install("commented_events.vsndevts", DEFAULT_REL);
    let before = std::fs::read(&path).unwrap();
    let (server, _state) = server_for(&tree);

    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [{"op": "remove", "name": "alpha.event"}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventInvalid");
    assert!(
        err.message.contains("beta.event"),
        "the refusal names the referrer: {}",
        err.message
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let out = call(
        &server,
        "edit_soundevents",
        json!({"force": true, "ops": [{"op": "remove", "name": "alpha.event"}]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("removed"));
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w["type"] == json!("removedReferenced") && w["referrers"] == json!(["beta.event"])),
        "force removed with a warning: {warnings:?}"
    );
    let parsed = cs2_kv3::parse(&tree.read(DEFAULT_REL)).unwrap();
    assert!(parsed.root.get("alpha.event").is_none());
}

#[tokio::test]
async fn paths_outside_the_addon_or_with_dotdot_refuse() {
    let tree = Tree::new("paths");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);

    let err = call(
        &server,
        "edit_soundevents",
        json!({"path": "csgo_addons/other_addon/soundevents/x.vsndevts",
               "ops": [set_op("a.b", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WriteOutsideAddon");

    let err = call(
        &server,
        "edit_soundevents",
        json!({"path": "csgo_addons/../x.vsndevts",
               "ops": [set_op("a.b", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments");

    let err = call(
        &server,
        "edit_soundevents",
        json!({"path": "csgo_addons/mcp_test/soundevents/x.vmd",
               "ops": [set_op("a.b", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.type_, "InvalidArguments", "a non-vsndevts extension is refused");
}

#[tokio::test]
async fn a_file_without_the_header_is_a_parse_error_at_line_1() {
    let tree = Tree::new("noheader");
    tree.write(DEFAULT_REL, "this is not kv3 at all\r\n");
    let (server, _state) = server_for(&tree);
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("a.b", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventsParseError");
    assert_eq!(err.data["line"], json!(1));
}

#[tokio::test]
async fn a_parse_error_reports_line_and_col_and_writes_nothing() {
    let tree = Tree::new("parse-err");
    let bad = format!(
        "{TEMPLATE_HEADER}\r\n{{\r\n\t\"ok.event\" = \r\n\t{{\r\n\t\ttype = \"csgo_mega\"\r\n\t}}\r\n\t=\r\n}}\r\n"
    );
    let path = tree.write(DEFAULT_REL, &bad);
    let before = std::fs::read(&path).unwrap();
    let (server, _state) = server_for(&tree);
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [set_op("a.b", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventsParseError");
    assert_eq!(err.data["line"], json!(7), "the lone `=` line");
    assert!(err.data["col"].as_u64().expect("col") >= 2);
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
}

#[tokio::test]
async fn argument_negatives_refuse_before_anything_happens() {
    let tree = Tree::new("args");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let (server, _state) = server_for(&tree);
    for args in [
        json!({"ops": []}),
        json!({}),
        json!({"ops": [set_op("a.b", json!({}))], "bogus": 1}),
        json!({"ops": [json!({"op": "explode", "name": "a.b"})]}),
        json!({"ops": [json!({"op": "set", "name": "a.b", "properties": {}, "wat": true})]}),
        json!({"ops": [json!({"op": "rename", "name": "alpha.event", "to": "alpha.event"})]}),
        json!({"dryRun": "yes", "ops": [set_op("a.b", json!({}))]}),
    ] {
        let err = call(&server, "edit_soundevents", args.clone()).await.unwrap_err();
        assert_eq!(err.type_, "InvalidArguments", "{args}");
    }
    // a removal of a name the file does not hold (with a near-miss suggestion), and a rename
    // onto an existing name
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [json!({"op": "remove", "name": "no.such.event"})]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventNotFound");
    assert_eq!(
        err.data["didYouMean"],
        json!(["alpha.event", "beta.event"]),
        "near-miss suggestions, ranked"
    );
    // a rename onto an existing name is SoundEventExists
    let err = call(
        &server,
        "edit_soundevents",
        json!({"ops": [json!({"op": "rename", "name": "alpha.event", "to": "beta.event"})]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventExists");
}

// -------------------------------------------------------------------------------------------
// seams
// -------------------------------------------------------------------------------------------

/// A resolver that knows one extra base (the tests' fixed seams; the index is never built).
fn resolver_knowing(name: &'static str) -> EditSeams {
    EditSeams {
        resolver: ResolverSource::Fixed(hammer_mcp::tools::soundevents_edit::BaseResolver {
            base: Arc::new(move |asked| if asked == name { Ok(()) } else { Err(Vec::new()) }),
            known_event: Arc::new(|_| None),
            known_keys: Arc::new(|| Some(["type", "base", "volume"].iter().map(|s| s.to_string()).collect())),
        }),
        splicer: hammer_mcp::tools::soundevents_edit::EditSeams::default().splicer,
    }
}

#[tokio::test]
async fn the_seam_resolver_answers_bases_the_file_does_not_hold() {
    let tree = Tree::new("seam-base");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let state = state_for(&tree);

    // default seams: the stub index refuses it
    let err = edit_with(
        &default_seams(),
        state.clone(),
        json!({"ops": [set_op("mcp.bad", json!({"base": "from.the.index"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "SoundEventBaseNotFound");

    // the injected index seam resolves it
    let out = edit_with(
        &resolver_knowing("from.the.index"),
        state,
        json!({"ops": [set_op("mcp.bad", json!({"base": "from.the.index"}))]}),
    )
    .await
    .unwrap();
    assert_eq!(out["ops"][0]["status"], json!("created"));
}

#[tokio::test]
async fn the_seam_resolver_flags_unknown_property_keys() {
    let tree = Tree::new("seam-keys");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let state = state_for(&tree);
    let out = edit_with(
        &resolver_knowing("from.the.index"),
        state,
        json!({"ops": [set_op("mcp.test", json!({"type": "csgo_mega", "not_a_real_key": 1}))]}),
    )
    .await
    .unwrap();
    let warnings = out["warnings"].as_array().expect("warnings");
    let unknown = warnings.iter().find(|w| w["type"] == json!("unknownKeys")).expect("unknownKeys");
    assert_eq!(unknown["entries"], json!([{"name": "mcp.test", "key": "not_a_real_key"}]));
}

#[tokio::test]
async fn a_broken_splice_is_an_internal_error_and_writes_nothing() {
    let tree = Tree::new("broken-splice");
    let path = tree.install("commented_events.vsndevts", DEFAULT_REL);
    let before = std::fs::read(&path).unwrap();
    let state = state_for(&tree);

    fn corrupted(_text: &str, _ops: &[SpliceOp]) -> Result<Spliced, SpliceError> {
        // parses fine, but is not what the ops asked for
        Ok(Spliced {
            text: format!("{TEMPLATE_HEADER}\r\n{{\r\n}}"),
            comments_dropped: Vec::new(),
        })
    }
    let seams = EditSeams { resolver: ResolverSource::Fixed(BaseResolver::default()), splicer: corrupted };

    let err = edit_with(
        &seams,
        state,
        json!({"ops": [set_op("mcp.test", json!({"type": "csgo_mega"}))]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InternalError");
    assert!(err.message.contains("nothing was written"), "{}", err.message);
    assert_eq!(std::fs::read(&path).unwrap(), before, "the corrupted splice never landed");
}

// -------------------------------------------------------------------------------------------
// Renames move only the key and the `base`/`soundevent_01` reference tokens; non-identifier
// names are quoted; argument errors win over the index build; the read-modify-write runs under
// the soundevents gate, so two concurrent edits of one file both land.
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_rename_leaves_a_property_value_holding_the_old_name_untouched() {
    // The old name also lives in another property (`notes`): since FIX-T the splice moves only
    // the key and the `base`/`soundevent_01` reference tokens, so the round trip holds and the
    // rename SUCCEEDS — the `notes` value must come out byte-for-byte (review N5: the old M2
    // refusal misattributed this to a splice bug).
    let tree = Tree::new("rename-cause");
    tree.write(
        DEFAULT_REL,
        concat!(
            "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\r\n",
            "{\r\n",
            "\t\"alpha.event\" = \r\n",
            "\t{\r\n",
            "\t\ttype = \"csgo_mega\"\r\n",
            "\t\tnotes = \"alpha.event\"\r\n",
            "\t}\r\n",
            "\t\"beta.event\" = \r\n",
            "\t{\r\n",
            "\t\tbase = \"alpha.event\"\r\n",
            "\t}\r\n",
            "}"
        ),
    );
    let state = state_for(&tree);

    let out = edit_with(
        &default_seams(),
        state,
        json!({"ops": [{"op": "rename", "name": "alpha.event", "to": "alpha.renamed"}]}),
    )
    .await
    .expect("the rename succeeds; the property value is not a live reference");

    assert_eq!(out["ops"][0]["status"], json!("renamed"));
    assert_eq!(out["referencesUpdated"], json!(1), "the `base` reference followed");
    assert_eq!(out["commentsDropped"], json!([]));
    assert!(out["warnings"].as_array().expect("warnings").is_empty());

    let text = tree.read(DEFAULT_REL);
    let parsed = cs2_kv3::parse(&text).expect("the renamed file re-parses");
    let alpha = parsed.root.get("alpha.renamed").and_then(cs2_kv3::Value::as_object)
        .expect("the event moved to its new name");
    assert_eq!(
        alpha.get("notes").and_then(cs2_kv3::Value::as_str),
        Some("alpha.event"),
        "the property value survived byte-for-byte"
    );
    let beta = parsed.root.get("beta.event").and_then(cs2_kv3::Value::as_object)
        .expect("beta.event");
    assert_eq!(beta.get("base").and_then(cs2_kv3::Value::as_str), Some("alpha.renamed"));
    assert!(text.contains("notes = \"alpha.event\""), "untouched in the raw text");
    // and the file no longer holds the old event key or the old base value
    assert!(text.contains("\"alpha.renamed\""));
    assert!(text.contains("base = \"alpha.renamed\""));
    assert!(!text.contains("base = \"alpha.event\""));
}

#[tokio::test]
async fn a_rename_to_a_digit_leading_name_quotes_the_key_and_reparses() {
    // review N6: renaming a bare-keyed event to `1up` used to emit a bare `1up` key, which the
    // KV3 lexer refuses (a bare key is `[A-Za-z_][A-Za-z0-9_.]*`) — the round trip then failed
    // as an InternalError. key_repr quotes every non-identifier name, so the rename lands.
    let tree = Tree::new("digit-name");
    tree.write(
        DEFAULT_REL,
        concat!(
            "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\r\n",
            "{\r\n",
            "\tbare_one = \r\n",
            "\t{\r\n",
            "\t\ttype = \"csgo_mega\"\r\n",
            "\t}\r\n",
            "}"
        ),
    );
    let state = state_for(&tree);

    let out = edit_with(
        &default_seams(),
        state,
        json!({"ops": [{"op": "rename", "name": "bare_one", "to": "1up"}]}),
    )
    .await
    .expect("the rename to a digit-leading name now succeeds");

    assert_eq!(out["ops"][0]["status"], json!("renamed"));
    let text = tree.read(DEFAULT_REL);
    assert!(
        text.contains("\t\"1up\" = \r\n"),
        "the key is emitted quoted: {text:?}"
    );
    let parsed = cs2_kv3::parse(&text).expect("the file re-parses (the key lexes)");
    assert!(
        parsed.root.get("1up").is_some(),
        "the event lives under the new name: {text:?}"
    );
}

#[tokio::test]
async fn a_valid_set_then_an_invalid_op_refuses_arguments_and_not_the_index() {
    // review N7: the unknown-key check used to build the soundevent index inside the ops loop,
    // so on an unbuildable index a later op's InvalidArguments was masked by the first op's
    // VpkMissing. The candidates are now resolved once after every op was arg-checked.
    let tree = Tree::new("op-order");
    tree.write(
        DEFAULT_REL,
        concat!(
            "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\r\n",
            "{\r\n",
            "}"
        ),
    );
    let state = state_for(&tree);

    let err = edit_with(
        &default_seams(),
        state,
        json!({
            "ops": [
                {"op": "set", "name": "mcp.valid", "properties": {"type": "csgo_mega"}},
                {"op": "explode", "name": "mcp.bad"}
            ]
        }),
    )
    .await
    .unwrap_err();

    assert_eq!(
        err.type_, "InvalidArguments",
        "the later op's argument error, not the first op's index build: {err:?}"
    );
    assert!(
        err.message.contains("explode"),
        "the message names the invalid op: {err:?}"
    );
    assert!(
        !err.message.contains("VpkMissing"),
        "not the first op's index build failure: {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_edits_of_one_file_both_land() {
    let tree = Tree::new("gate");
    tree.install("commented_events.vsndevts", DEFAULT_REL);
    let state = state_for(&tree);
    let (state1, state2) = (state.clone(), state.clone());

    // Two tasks real-race for the write gate; the loser waits, re-READS the file (finding the
    // winner's event in it) and lands on top of it — a lost update would drop one event.
    let first = tokio::spawn(async move {
        edit_with(
            &EditSeams::default(),
            state1,
            json!({"ops": [set_op("mcp.gate.one", json!({"type": "csgo_mega"}))]}),
        )
        .await
    });
    let second = tokio::spawn(async move {
        edit_with(
            &EditSeams::default(),
            state2,
            json!({"ops": [set_op("mcp.gate.two", json!({"type": "csgo_mega"}))]}),
        )
        .await
    });
    let (one, two) = tokio::join!(first, second);
    let one = one.expect("task 1").expect("edit 1 landed");
    let two = two.expect("task 2").expect("edit 2 landed");
    assert_eq!(one["written"], json!(true));
    assert_eq!(two["written"], json!(true));

    let parsed = cs2_kv3::parse(&tree.read(DEFAULT_REL)).expect("the file still parses");
    assert!(
        parsed.root.get("mcp.gate.one").is_some(),
        "the first edit survived the second (the reviews' lost update)"
    );
    assert!(
        parsed.root.get("mcp.gate.two").is_some(),
        "the second edit landed (and read the first's result)"
    );
}
