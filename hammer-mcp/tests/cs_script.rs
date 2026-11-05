//! The `cs_script` tool (`src/tools/cs_script.rs`, `src/cs_script/`) against temp addon trees
//! under `CARGO_TARGET_TMPDIR`. The default tests run npm/node as FAKES (bash scripts the tree
//! writes; `CS_SCRIPT_NPM` / `CS_SCRIPT_NODE` point at them) so nothing needs the network or a
//! TypeScript install; the `#[ignore]` tests use the real npm/node/rollup/tsc and the network:
//!
//! ```sh
//! cargo test -p hammer-mcp --test cs_script -- --ignored --nocapture --test-threads=1
//! ```
#![allow(clippy::result_large_err)]

use clap::Parser;
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::McpServer;
use cs2_mcp_common::mock::jobs::MockJobOutcome;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use hammer_mcp::cs_script::{self, diag, types, Project};
use hammer_mcp::state::HammerState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);
/// The fake/real program selection is process-global (env vars): tests that start programs hold this.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const GAME_D_TS: &str = "/** the game's point_script.d.ts (test copy) */\ndeclare module \"cs_script/point_script\" {\n    export const Instance: { Msg(text: string): void };\n}\n";

struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "cs-script-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test/maps")).expect("tmp tree");
        Tree { root, app }
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    fn project(&self) -> PathBuf {
        self.addon().join("cs_script")
    }

    fn game_types(&self, text: &str) {
        let path = types::game_file(&self.app);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn config(&self, discovery: Option<&Path>) -> CommonConfig {
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let root = self.root.display().to_string();
        let map = format!("z:\\app={}", self.app.display());
        let mut args: Vec<String> = vec![
            "--deadline-ms".into(),
            "5000".into(),
            "--tool-budget-ms".into(),
            "60000".into(),
            "--project-root".into(),
            root,
            "--path-map".into(),
            map,
        ];
        if let Some(discovery) = discovery {
            args.extend(["--discovery".into(), discovery.display().to_string()]);
        }
        let given: Vec<&str> = args.iter().map(String::as_str).collect();
        let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
        W::parse_from(argv).c
    }

    fn server(&self) -> McpServer<HammerState> {
        hammer_mcp::build_server(&self.config(None)).expect("server")
    }

    /// Fake npm and node (see the module header); returns nothing, sets the env vars.
    /// * `npm install` creates node_modules (the six packages at 9.9.9, fake tsc/rollup shims) and
    ///   copies `<root>/npm-types.d.ts` to the package's autoimport declaration;
    /// * `node` as tsc prints `<root>/tsc.out` and exits with the code in `<root>/tsc.code`;
    ///   as rollup writes `<root>/rollup.js` (plus the target name) to the OUTPUT_DIR of the cwd's
    ///   rollup.config.js; with `--input-type` prints `<root>/fingerprint.json`.
    fn fakes(&self) {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let root = self.root.display().to_string();
        let npm = format!(
            r#"#!/bin/bash
case "$1" in
  install)
    mkdir -p node_modules/.bin node_modules/@s2ze/types/build/autoimport
    for p in debug math prototype-extend scheduler tsconfig types; do
      mkdir -p node_modules/@s2ze/$p; echo '{{"version":"9.9.9"}}' > node_modules/@s2ze/$p/package.json
    done
    cp '{root}/npm-types.d.ts' node_modules/@s2ze/types/build/autoimport/index.d.ts
    echo fake > node_modules/.bin/tsc; echo fake > node_modules/.bin/rollup
    echo "$@" >> '{root}/npm.log'
    echo "added 7 packages"
    ;;
  view) echo 9.9.9 ;;
esac
"#
        );
        let node = format!(
            r#"#!/bin/bash
case "$1" in
  --input-type=*) cat '{root}/fingerprint.json' ;;
  */tsc)
    cat '{root}/tsc.out' 2>/dev/null
    exit "$(cat '{root}/tsc.code' 2>/dev/null || echo 0)"
    ;;
  */rollup)
    out=$(sed -n "s/^const OUTPUT_DIR = \"\(.*\)\";/\1/p" rollup.config.js)
    mkdir -p "$out"
    echo "// GENERATED" > "$out/$CS_SCRIPT_ONLY.js"
    cat '{root}/rollup.js' >> "$out/$CS_SCRIPT_ONLY.js"
    ;;
esac
"#
        );
        for (name, text) in [("npm", npm), ("node", node)] {
            let path = bin.join(name);
            std::fs::write(&path, text).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        std::fs::write(self.root.join("npm-types.d.ts"), GAME_D_TS).unwrap();
        std::fs::write(self.root.join("rollup.js"), "export {};\n").unwrap();
        std::fs::write(self.root.join("tsc.code"), "0").unwrap();
        std::env::set_var("CS_SCRIPT_NPM", bin.join("npm"));
        std::env::set_var("CS_SCRIPT_NODE", bin.join("node"));
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn call(server: &McpServer<HammerState>, args: Value) -> Result<Value, ToolError> {
    match server.call("cs_script", args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path.as_ref()).unwrap_or_else(|e| panic!("{}: {e}", path.as_ref().display()))
}

// -------------------------------------------------------------------------------------------
// Pure pieces
// -------------------------------------------------------------------------------------------

#[test]
fn names_are_restricted() {
    for ok in ["main", "ti_convoy", "a1", "x_"] {
        assert!(cs_script::valid_name(ok), "{ok}");
    }
    for bad in ["", "Main", "a-b", "a b", "../x", "a/b", "ä", &"x".repeat(49)] {
        assert!(!cs_script::valid_name(bad), "{bad:?}");
    }
    assert_eq!(cs_script::package_name("mcp_test"), "cs-script-mcp_test");
    assert_eq!(cs_script::package_name("My Addon!"), "cs-script-my-addon");
}

#[test]
fn scaffold_writes_the_boilerplate_with_the_output_dir_substituted() {
    let tree = Tree::new("scaffold");
    let project = Project::new("mcp_test", &tree.addon());
    let written = cs_script::scaffold(&project, &["main".into(), "other".into()]).unwrap();
    for rel in [
        "package.json",
        "rollup.config.js",
        ".gitignore",
        "README.md",
        "LICENSE.create-cs-script",
        "src/main/index.ts",
        "src/main/tsconfig.json",
        "src/other/index.ts",
        "src/other/tsconfig.json",
    ] {
        assert!(written.iter().any(|w| w == rel), "{rel} in {written:?}");
        assert!(tree.project().join(rel).is_file(), "{rel}");
    }
    let rollup = read(tree.project().join("rollup.config.js"));
    let out_dir = tree.addon().join("maps/scripts");
    assert!(rollup.contains(&format!("const OUTPUT_DIR = \"{}\";", out_dir.display())), "{rollup}");
    assert!(!rollup.contains("__"), "no placeholder left: {rollup}");
    assert_eq!(project.targets().unwrap(), ["main", "other"]);
    assert!(rollup.contains("output: `${OUTPUT_DIR}/other.js`"));
    let package: Value = serde_json::from_str(&read(tree.project().join("package.json"))).unwrap();
    assert_eq!(package["name"], json!("cs-script-mcp_test"));
    assert_eq!(package["type"], json!("module"));
    assert!(package["dependencies"]["@s2ze/types"].is_string());
    let readme = read(tree.project().join("README.md"));
    assert!(!readme.contains("__") && readme.contains("mcp_test") && readme.contains(cs_script::UPSTREAM_VERSION), "{readme}");
    assert!(read(tree.project().join("src/main/index.ts")).contains("[main] loaded"));
    assert!(read(tree.project().join("LICENSE.create-cs-script")).contains("MIT License"));
    // a second scaffold is refused and changes nothing
    let before = read(tree.project().join("rollup.config.js"));
    assert!(matches!(cs_script::scaffold(&project, &["main".into()]), Err(cs_script::CsError::Exists(_))));
    assert_eq!(read(tree.project().join("rollup.config.js")), before);
    // bad / repeated names are refused before anything is written
    let other = Tree::new("scaffold-bad");
    let p2 = Project::new("mcp_test", &other.addon());
    assert!(cs_script::scaffold(&p2, &["Bad".into()]).is_err());
    assert!(cs_script::scaffold(&p2, &["a".into(), "a".into()]).is_err());
    assert!(!other.project().exists());
}

#[test]
fn add_edits_the_targets_array_and_refuses_duplicates_and_bad_names() {
    let tree = Tree::new("add");
    let project = Project::new("mcp_test", &tree.addon());
    cs_script::scaffold(&project, &["main".into()]).unwrap();
    let written = cs_script::add_script(&project, "second_one").unwrap();
    assert!(written.contains(&"src/second_one/index.ts".to_string()) && written.contains(&"rollup.config.js".to_string()));
    assert_eq!(project.targets().unwrap(), ["main", "second_one"]);
    let rollup = read(tree.project().join("rollup.config.js"));
    assert!(rollup.contains("// cs-script:targets:end"), "markers survive");
    let order = [rollup.find("name: 'main'").unwrap(), rollup.find("name: 'second_one'").unwrap(), rollup.find("targets:end").unwrap()];
    assert!(order[0] < order[1] && order[1] < order[2], "{rollup}");
    assert!(matches!(cs_script::add_script(&project, "main"), Err(cs_script::CsError::Duplicate(_))));
    assert!(matches!(cs_script::add_script(&project, "second_one"), Err(cs_script::CsError::Duplicate(_))));
    for bad in ["Has Space", "UP", "a-b", "../x", ""] {
        assert!(matches!(cs_script::add_script(&project, bad), Err(cs_script::CsError::BadName(_))), "{bad:?}");
    }
    assert_eq!(project.targets().unwrap(), ["main", "second_one"], "refusals change nothing");
    // a src folder without a target is not silently adopted
    std::fs::create_dir_all(tree.project().join("src/orphan")).unwrap();
    assert!(cs_script::add_script(&project, "orphan").is_err());
    // the markers are what the editing relies on: without them the tool says so
    std::fs::write(tree.project().join("rollup.config.js"), "export default [];\n").unwrap();
    assert!(matches!(cs_script::add_script(&project, "third"), Err(cs_script::CsError::Config(_))));
}

#[test]
fn source_paths_cannot_leave_the_script_folder() {
    let tree = Tree::new("paths");
    let project = Project::new("mcp_test", &tree.addon());
    let ok = project.source_path("main", "util/helpers.ts").unwrap();
    assert!(ok.ends_with("cs_script/src/main/util/helpers.ts"));
    for bad in ["", "../x.ts", "a/../x.ts", "/etc/x.ts", "a\\b.ts", "x.js", "x.json", "x.ts/", ".ts/..", "a b.ts", "x\n.ts", "./x.ts", "a//b.ts"] {
        assert!(project.source_path("main", bad).is_err(), "{bad:?} must be refused");
    }
    assert!(project.source_path("../main", "x.ts").is_err());
    assert!(project.source_path("Main", "x.ts").is_err());
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_addon_is_refused() {
    let tree = Tree::new("symlink");
    let outside = Tree::new("symlink-outside");
    let project = Project::new("mcp_test", &tree.addon());
    cs_script::scaffold(&project, &["main".into()]).unwrap();
    std::os::unix::fs::symlink(&outside.root, tree.project().join("src/main/evil")).unwrap();
    let path = project.source_path("main", "evil/x.ts").unwrap();
    assert!(cs_script::inside_addon(&tree.addon(), &path).is_err());
    // a dangling link is refused too, not followed
    std::os::unix::fs::symlink(outside.root.join("nowhere"), tree.project().join("src/main/dangling")).unwrap();
    let path = project.source_path("main", "dangling/x.ts").unwrap();
    assert!(cs_script::inside_addon(&tree.addon(), &path).is_err());
    // the project dir itself as a link out
    let other = Tree::new("symlink-dir");
    std::os::unix::fs::symlink(&outside.root, other.addon().join("cs_script")).unwrap();
    let p2 = Project::new("mcp_test", &other.addon());
    assert!(cs_script::scaffold(&p2, &["main".into()]).is_err());
    assert!(std::fs::read_dir(&outside.root).unwrap().all(|e| e.unwrap().file_name() != "package.json"));
}

const TSC_FIXTURE: &str = "src/main/index.ts(3,14): error TS2339: Property 'Foo' does not exist on type '{ Msg(text: string): void; }'.\n\
src/main/util.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.\n  The expected type comes from property 'x' which is declared here on type 'P'\n\
error TS18003: No inputs were found in config file 'src/x/tsconfig.json'.\n\
Found 3 errors in 2 files.\n";

#[test]
fn tsc_output_parses_into_positioned_diagnostics() {
    let diags = diag::parse_tsc(TSC_FIXTURE);
    assert_eq!(diags.len(), 3, "{diags:?}");
    assert_eq!(
        (diags[0].file.as_str(), diags[0].line, diags[0].col, diags[0].code.as_str()),
        ("src/main/index.ts", 3, 14, "TS2339")
    );
    assert!(diags[0].message.starts_with("Property 'Foo' does not exist"));
    assert_eq!((diags[1].line, diags[1].col, diags[1].code.as_str()), (10, 5, "TS2322"));
    assert!(diags[1].message.contains("The expected type comes from property"), "continuation lines join: {}", diags[1].message);
    assert_eq!((diags[2].file.as_str(), diags[2].line, diags[2].code.as_str()), ("", 0, "TS18003"));
    let listing = diag::listing(&diags);
    assert_eq!(listing["errorCount"], json!(3));
    assert_eq!(listing["truncated"], json!(false));
    assert_eq!(listing["errors"][0]["file"], json!("src/main/index.ts"));
    // capped
    let many: String = (0..50).map(|i| format!("src/a.ts({i},1): error TS1005: ';' expected.\n")).collect();
    let listing = diag::listing(&diag::parse_tsc(&many));
    assert_eq!(listing["errorCount"], json!(50));
    assert_eq!(listing["errors"].as_array().unwrap().len(), diag::MAX_LISTED);
    assert_eq!(listing["truncated"], json!(true));
    // rollup: a plugin error without TS positions is one ROLLUP entry with its location
    let rollup = "\n[!] RollupError: Could not resolve './missing' from src/main/index.ts\nsrc/main/index.ts (4:8)\n2: import x from './missing';\n";
    let diags = diag::parse_rollup(rollup);
    assert_eq!(diags.len(), 1);
    assert_eq!((diags[0].code.as_str(), diags[0].file.as_str(), diags[0].line, diags[0].col), ("ROLLUP", "src/main/index.ts", 4, 8));
    assert!(diags[0].message.contains("Could not resolve"));
}

#[test]
fn the_override_follows_the_drift_verdict_and_back() {
    let tree = Tree::new("override");
    let project = Project::new("mcp_test", &tree.addon());
    cs_script::scaffold(&project, &["main".into(), "other".into()]).unwrap();
    let dir = tree.project();
    let plain = read(dir.join("src/main/tsconfig.json"));
    let different = types::Verdict::Different(json!({"onlyInNpm": ["Console"]}));

    // equal, nothing there: nothing to do
    assert_eq!(types::sync_override(&dir, &types::Verdict::Equal, Some(GAME_D_TS)).unwrap().0, types::OverrideAction::None);
    assert!(!types::override_active(&dir));
    // unknown never touches anything
    assert_eq!(types::sync_override(&dir, &types::Verdict::Unknown("offline".into()), None).unwrap().0, types::OverrideAction::Untouched);

    // different: the game's file is copied in and every script tsconfig is rewired
    let (action, rewired, unreadable) = types::sync_override(&dir, &different, Some(GAME_D_TS)).unwrap();
    assert_eq!(action, types::OverrideAction::Written);
    assert_eq!(rewired, ["main", "other"]);
    assert!(unreadable.is_empty());
    assert_eq!(read(dir.join(types::OVERRIDE_REL)), GAME_D_TS);
    for name in ["main", "other"] {
        let config: Value = serde_json::from_str(&read(dir.join(format!("src/{name}/tsconfig.json")))).unwrap();
        assert_eq!(config["extends"], json!("@s2ze/tsconfig"));
        assert_eq!(config["compilerOptions"]["types"], json!([]), "the npm autoimport types are no longer loaded");
        assert_eq!(config["include"], json!(["../../types/point_script.d.ts", "./**/*"]));
    }
    // a script added now follows the override
    cs_script::add_script(&project, "third").unwrap();
    let third: Value = serde_json::from_str(&read(dir.join("src/third/tsconfig.json"))).unwrap();
    assert_eq!(third["compilerOptions"]["types"], json!([]));
    // same verdict again: kept, and a changed game file refreshes the copy
    assert_eq!(types::sync_override(&dir, &different, Some(GAME_D_TS)).unwrap().0, types::OverrideAction::Kept);
    let newer = format!("{GAME_D_TS}// newer\n");
    assert_eq!(types::sync_override(&dir, &different, Some(&newer)).unwrap().0, types::OverrideAction::Refreshed);
    assert_eq!(read(dir.join(types::OVERRIDE_REL)), newer);

    // the package caught up: the file and the rewiring go, the tsconfigs are the boilerplate's again
    let (action, rewired, _) = types::sync_override(&dir, &types::Verdict::Equal, Some(&newer)).unwrap();
    assert_eq!(action, types::OverrideAction::Removed);
    assert_eq!(rewired, ["main", "other", "third"]);
    assert!(!types::override_active(&dir));
    assert!(!dir.join("types").exists(), "the empty types folder is removed");
    for name in ["main", "other", "third"] {
        let config: Value = serde_json::from_str(&read(dir.join(format!("src/{name}/tsconfig.json")))).unwrap();
        assert_eq!(config, json!({"extends": "@s2ze/tsconfig"}), "{name}");
    }
    assert_eq!(serde_json::from_str::<Value>(&plain).unwrap(), json!({"extends": "@s2ze/tsconfig"}));
}

#[test]
fn whitespace_does_not_count_as_drift() {
    let a = "declare module \"x\" {  \r\n  const a: 1;\t\r\n}\r\n\r\n";
    let b = "declare module \"x\" {\n  const a: 1;\n}";
    assert_eq!(types::normalise(a), types::normalise(b));
    assert_ne!(types::normalise(a), types::normalise("declare module \"x\" { const a: 2; }"));
}

#[test]
fn a_fingerprint_report_becomes_a_verdict() {
    assert_eq!(types::verdict_from_report("{\"equal\":true,\"onlyA\":[],\"onlyB\":[],\"changed\":[]}\n"), types::Verdict::Equal);
    match types::verdict_from_report("noise\n{\"equal\":false,\"onlyA\":[\"A\"],\"onlyB\":[\"B\"],\"changed\":[],\"counts\":{}}\n") {
        types::Verdict::Different(v) => {
            assert_eq!(v["onlyInGame"], json!(["A"]));
            assert_eq!(v["onlyInNpm"], json!(["B"]));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(types::verdict_from_report("TypeError: boom"), types::Verdict::Unknown(_)));
}

// -------------------------------------------------------------------------------------------
// The tool, with fake npm/node
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn init_scaffolds_installs_checks_the_types_and_refuses_a_second_init() {
    let _env = ENV.lock().await;
    let tree = Tree::new("init");
    tree.fakes();
    tree.game_types(GAME_D_TS);
    let server = tree.server();

    let out = call(&server, json!({"action": "init"})).await.expect("init");
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["done"], json!(true));
    assert_eq!(out["scripts"], json!(["main"]));
    assert_eq!(out["addon"], json!("mcp_test"));
    assert_eq!(out["install"]["ok"], json!(true));
    assert_eq!(out["versions"]["@s2ze/types"], json!("9.9.9"));
    assert_eq!(out["types"]["verdict"], json!("equal"));
    assert_eq!(out["types"]["override"], json!("none"));
    assert!(tree.project().join("src/main/index.ts").is_file());
    let log = read(tree.root.join("npm.log"));
    assert!(log.contains("@s2ze/types@latest") && log.contains("@s2ze/prototype-extend@latest"), "{log}");
    assert!(out["created"].as_array().unwrap().iter().any(|v| v == "rollup.config.js"));

    let err = call(&server, json!({"action": "init"})).await.unwrap_err();
    assert_eq!(err.name, "ProjectExists", "{err:?}");
    assert_eq!(read(tree.root.join("npm.log")).lines().count(), 1, "the refusal ran no npm");

    // arguments that do not belong to the action are refused
    let err = call(&server, json!({"action": "add", "name": "x", "content": "y"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    let err = call(&server, json!({"action": "nope"})).await.unwrap_err();
    assert_eq!(err.name, "InvalidParams");
}

#[tokio::test]
async fn a_failed_install_keeps_the_files_and_says_so() {
    let _env = ENV.lock().await;
    let tree = Tree::new("init-fail");
    tree.fakes();
    std::env::set_var("CS_SCRIPT_NPM", tree.root.join("bin/no-such-npm"));
    let server = tree.server();
    let out = call(&server, json!({"action": "init", "scripts": ["a", "b"]})).await.expect("init");
    assert_eq!(out["ok"], json!(false));
    assert!(out["install"]["error"].as_str().unwrap().contains("cannot start"), "{out}");
    assert_eq!(out["scripts"], json!(["a", "b"]));
    assert!(tree.project().join("rollup.config.js").is_file());
    assert_eq!(out["next"][0]["args"]["action"], json!("update"), "{out}");
}

#[tokio::test]
async fn update_writes_and_removes_the_types_override_with_the_drift_verdict() {
    let _env = ENV.lock().await;
    let tree = Tree::new("update");
    tree.fakes();
    tree.game_types(GAME_D_TS);
    let server = tree.server();
    call(&server, json!({"action": "init", "scripts": ["main", "other"]})).await.unwrap();
    assert!(!types::override_active(&tree.project()));

    // the package now declares something else (the node fingerprint says what)
    std::fs::write(tree.root.join("npm-types.d.ts"), "declare module \"cs_script/point_script\" { export const Console: 1; }\n").unwrap();
    std::fs::write(
        tree.root.join("fingerprint.json"),
        "{\"equal\":false,\"onlyA\":[],\"onlyB\":[\"Console\"],\"changed\":[],\"counts\":{\"onlyA\":0,\"onlyB\":1,\"changed\":0}}\n",
    )
    .unwrap();
    let out = call(&server, json!({"action": "update"})).await.expect("update");
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["before"]["@s2ze/types"], json!("9.9.9"));
    assert_eq!(out["types"]["verdict"], json!("different"), "{out}");
    assert_eq!(out["types"]["override"], json!("written"));
    assert_eq!(out["types"]["differences"]["onlyInNpm"], json!(["Console"]));
    assert!(out["types"]["warning"].as_str().unwrap().contains("types/point_script.d.ts"));
    assert_eq!(read(tree.project().join(types::OVERRIDE_REL)), GAME_D_TS);
    let config: Value = serde_json::from_str(&read(tree.project().join("src/other/tsconfig.json"))).unwrap();
    assert_eq!(config["compilerOptions"]["types"], json!([]));

    // status reports it
    let status = call(&server, json!({"action": "status"})).await.unwrap();
    assert_eq!(status["types"]["overrideActive"], json!(true));
    assert_eq!(status["types"]["npmVsGame"], json!("different"));

    // a script added meanwhile follows the override
    call(&server, json!({"action": "add", "name": "third"})).await.unwrap();
    assert!(read(tree.project().join("src/third/tsconfig.json")).contains("point_script.d.ts"));

    // the package matches the game again: the override goes
    std::fs::write(tree.root.join("npm-types.d.ts"), GAME_D_TS).unwrap();
    let out = call(&server, json!({"action": "update"})).await.expect("update");
    assert_eq!(out["types"]["verdict"], json!("equal"), "{out}");
    assert_eq!(out["types"]["override"], json!("removed"));
    assert!(!types::override_active(&tree.project()));
    assert_eq!(
        serde_json::from_str::<Value>(&read(tree.project().join("src/third/tsconfig.json"))).unwrap(),
        json!({"extends": "@s2ze/tsconfig"})
    );

    // the game's file missing: unknown, and nothing is touched
    std::fs::remove_file(types::game_file(&tree.app)).unwrap();
    let out = call(&server, json!({"action": "update"})).await.unwrap();
    assert_eq!(out["types"]["verdict"], json!("unknown"));
    assert!(out["types"]["reason"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn write_and_read_only_touch_ts_sources_of_a_known_script() {
    let _env = ENV.lock().await;
    let tree = Tree::new("write");
    tree.fakes();
    let server = tree.server();
    let err = call(&server, json!({"action": "write", "name": "main", "content": "x"})).await.unwrap_err();
    assert_eq!(err.name, "NoProject", "{err:?}");
    call(&server, json!({"action": "init"})).await.unwrap();

    let out = call(&server, json!({"action": "write", "name": "main", "content": "export const x = 1;\n"})).await.unwrap();
    assert_eq!(out["path"], json!("cs_script/src/main/index.ts"));
    assert_eq!(out["created"], json!(false));
    assert_eq!(read(tree.project().join("src/main/index.ts")), "export const x = 1;\n");
    let out = call(&server, json!({"action": "write", "name": "main", "file": "lib/util.ts", "content": "export {};\n"})).await.unwrap();
    assert_eq!(out["created"], json!(true));
    assert_eq!(out["files"], json!(["index.ts", "lib/util.ts"]));
    let out = call(&server, json!({"action": "read", "name": "main", "file": "lib/util.ts"})).await.unwrap();
    assert_eq!(out["content"], json!("export {};\n"));
    let out = call(&server, json!({"action": "read", "name": "main"})).await.unwrap();
    assert_eq!(out["content"], json!("export const x = 1;\n"));

    for (name, file) in [
        ("main", "../../rollup.config.js"),
        ("main", "x.js"),
        ("main", "tsconfig.json"),
        ("main", "/etc/passwd.ts"),
        ("main", "a/../../x.ts"),
        ("../main", "x.ts"),
    ] {
        let err = call(&server, json!({"action": "write", "name": name, "file": file, "content": "x"})).await.unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{name} {file}: {err:?}");
    }
    let err = call(&server, json!({"action": "write", "name": "ghost", "content": "x"})).await.unwrap_err();
    assert_eq!(err.name, "UnknownScript");
    assert_eq!(err.data["scripts"], json!(["main"]));
    let big = "x".repeat(cs_script::SOURCE_MAX_BYTES + 1);
    assert_eq!(call(&server, json!({"action": "write", "name": "main", "content": big})).await.unwrap_err().name, "InvalidParams");
    assert_eq!(call(&server, json!({"action": "read", "name": "main", "file": "nope.ts"})).await.unwrap_err().name, "FileNotFound");
    assert!(!tree.addon().join("rollup.config.js").exists());
}

#[tokio::test]
async fn build_reports_type_errors_then_writes_the_js_and_says_whether_it_changed() {
    let _env = ENV.lock().await;
    let tree = Tree::new("build");
    tree.fakes();
    let server = tree.server();
    call(&server, json!({"action": "init", "scripts": ["main", "other"]})).await.unwrap();

    // a type error: parsed, no bundle written
    std::fs::write(tree.root.join("tsc.out"), TSC_FIXTURE).unwrap();
    std::fs::write(tree.root.join("tsc.code"), "2").unwrap();
    let out = call(&server, json!({"action": "build", "name": "main"})).await.expect("build");
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(out["built"], json!([]));
    let failed = &out["failed"][0];
    assert_eq!(failed["name"], json!("main"));
    assert_eq!(failed["stage"], json!("typecheck"));
    assert_eq!(failed["errorCount"], json!(3));
    assert_eq!(failed["errors"][0]["code"], json!("TS2339"));
    assert_eq!(failed["errors"][0]["line"], json!(3));
    assert_eq!(failed["errors"][0]["file"], json!("src/main/index.ts"));
    assert!(!tree.addon().join("maps/scripts/main.js").exists());
    assert_eq!(out["next"][0]["args"]["action"], json!("read"));

    // fixed: both scripts build; first time the file is new
    std::fs::write(tree.root.join("tsc.code"), "0").unwrap();
    std::fs::write(tree.root.join("tsc.out"), "").unwrap();
    let out = call(&server, json!({"action": "build"})).await.expect("build");
    assert_eq!(out["ok"], json!(true), "{out}");
    let built = out["built"].as_array().unwrap();
    assert_eq!(built.len(), 2);
    assert_eq!(built[0]["name"], json!("main"));
    assert_eq!(built[0]["changed"], json!(true));
    assert_eq!(built[0]["existed"], json!(false));
    assert_eq!(built[0]["outputRel"], json!("maps/scripts/main.js"));
    assert!(read(tree.addon().join("maps/scripts/main.js")).starts_with("// GENERATED"));
    assert_eq!(out["keyvalue"], json!(["maps/scripts/main.vjs", "maps/scripts/other.vjs"]));
    // no map in the tools session: reloaded stays unknown, with the reason
    assert_eq!(out["reload"]["reloaded"], json!("unknown"));

    // the same sources again: nothing changed
    let out = call(&server, json!({"action": "build", "name": "main"})).await.unwrap();
    assert_eq!(out["built"][0]["changed"], json!(false));
    // new output: changed
    std::fs::write(tree.root.join("rollup.js"), "export const y = 2;\n").unwrap();
    let out = call(&server, json!({"action": "build", "name": "main"})).await.unwrap();
    assert_eq!(out["built"][0]["changed"], json!(true));
    assert_eq!(out["built"][0]["existed"], json!(true));

    // status: built fresh, compiled missing, project facts
    let status = call(&server, json!({"action": "status"})).await.unwrap();
    assert_eq!(status["project"]["present"], json!(true));
    let scripts = status["scripts"].as_array().unwrap();
    assert_eq!(scripts[0]["name"], json!("main"));
    assert_eq!(scripts[0]["built"], json!("fresh"));
    assert_eq!(scripts[0]["compiled"], json!("missing"));
    assert_eq!(scripts[0]["keyvalue"], json!("maps/scripts/main.vjs"));
    assert_eq!(status["packages"]["installed"]["@s2ze/types"], json!("9.9.9"));
    assert_eq!(status["packages"]["latestTypes"], json!("9.9.9"));
    assert_eq!(status["packages"]["outdated"], json!(false));
    // touching a source makes it stale
    std::thread::sleep(std::time::Duration::from_millis(1100));
    call(&server, json!({"action": "write", "name": "main", "content": "export {};\n"})).await.unwrap();
    let status = call(&server, json!({"action": "status"})).await.unwrap();
    assert_eq!(status["scripts"][0]["built"], json!("stale"));
}

#[tokio::test]
async fn status_without_a_project_points_at_init() {
    let _env = ENV.lock().await;
    let tree = Tree::new("status");
    tree.fakes();
    let out = call(&tree.server(), json!({"action": "status"})).await.unwrap();
    assert_eq!(out["project"]["present"], json!(false));
    assert_eq!(out["next"][0]["args"]["action"], json!("init"));
}

#[tokio::test]
async fn an_unknown_addon_folder_is_refused_not_created() {
    let _env = ENV.lock().await;
    let tree = Tree::new("noaddon");
    tree.fakes();
    std::fs::remove_dir_all(tree.addon()).unwrap();
    let err = call(&tree.server(), json!({"action": "init"})).await.unwrap_err();
    assert_eq!(err.name, "AddonNotFound", "{err:?}");
    assert!(!tree.addon().exists());
}

#[tokio::test]
async fn compile_goes_through_compile_asset_with_the_js_input() {
    let _env = ENV.lock().await;
    let tree = Tree::new("compile");
    tree.fakes();
    let vjs_c = tree.app.join("game/csgo_addons/mcp_test/maps/scripts/main.vjs_c");
    let mb = MockBridge::builder().job_script(MockJobOutcome::Ok, 0).job_writes([vjs_c.clone()]).start().await.unwrap();
    let server = hammer_mcp::build_server(&tree.config(Some(&mb.discovery_path))).expect("server");
    call(&server, json!({"action": "init"})).await.unwrap();

    // not built yet: a clear refusal that names the fix
    let err = call(&server, json!({"action": "compile", "name": "main"})).await.unwrap_err();
    assert_eq!(err.name, "ScriptNotBuilt", "{err:?}");
    call(&server, json!({"action": "build", "name": "main"})).await.unwrap();

    let out = call(&server, json!({"action": "compile", "name": "main"})).await.expect("compile");
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["compiled"][0]["state"], json!("ok"));
    assert_eq!(out["compiled"][0]["vjs_c"]["exists"], json!(true));
    let starts: Vec<Value> = mb.method_calls("job.start").into_iter().map(|r| r.params).collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["input"], json!("csgo_addons/mcp_test/maps/scripts/main.js"));
    assert_eq!(starts[0]["flags"], json!(["-nop4"]));
    assert_eq!(starts[0]["game"], json!(true));
}

#[tokio::test]
async fn compile_asset_accepts_a_js_path() {
    let _env = ENV.lock().await;
    let tree = Tree::new("compile-asset");
    tree.fakes();
    let js = tree.addon().join("maps/scripts/main.js");
    std::fs::create_dir_all(js.parent().unwrap()).unwrap();
    std::fs::write(&js, "export {};").unwrap();
    let mb = MockBridge::builder().job_script(MockJobOutcome::Ok, 0).start().await.unwrap();
    let server = hammer_mcp::build_server(&tree.config(Some(&mb.discovery_path))).expect("server");
    let out = match server.call("compile_asset", json!({"path": "maps/scripts/main.js"})).await {
        Ok(out) => out.json,
        Err(CallFailure::Tool(e)) => panic!("{e:?}"),
        Err(CallFailure::UnknownTool(n)) => panic!("{n}"),
    };
    assert_eq!(out["state"], json!("running"));
    assert_eq!(out["input"], json!("csgo_addons/mcp_test/maps/scripts/main.js"));
    // other extensions are still refused host-side
    let err = match server.call("compile_asset", json!({"path": "maps/scripts/main.ts"})).await {
        Err(CallFailure::Tool(e)) => e,
        other => panic!("{other:?}"),
    };
    assert!(err.message.contains("not a compilable asset"), "{err:?}");
}

// -------------------------------------------------------------------------------------------
// Real npm / node / rollup / tsc (network): `--ignored`
// -------------------------------------------------------------------------------------------

/// The whole loop with the real toolchain against the real game types: init (npm install from the
/// registry), a script that builds, one with a type error (parsed), the types verdict against the
/// game's own point_script.d.ts and, if it differs from npm, a script using an npm-only API fails
/// to type-check because of the override. Needs the game install at `workshop/run/app` (the
/// project root's) for the game's declaration; set `CS2_APP` to use another.
#[tokio::test]
#[ignore = "needs npm, node and the network; cargo test -p hammer-mcp --test cs_script -- --ignored --nocapture"]
async fn real_npm_roundtrip() {
    let _env = ENV.lock().await;
    std::env::remove_var("CS_SCRIPT_NPM");
    std::env::remove_var("CS_SCRIPT_NODE");
    let tree = Tree::new("real");
    let app = std::env::var("CS2_APP").unwrap_or_else(|_| {
        let root = std::env::var("CS2_MCP_PROJECT_ROOT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/..").to_string());
        format!("{root}/workshop/run/app")
    });
    let game = types::game_file(Path::new(&app));
    tree.game_types(&std::fs::read_to_string(&game).unwrap_or_else(|e| panic!("{}: {e}", game.display())));
    let server = tree.server();

    let out = call(&server, json!({"action": "init", "waitMs": 240_000})).await.expect("init");
    println!("init: {}", serde_json::to_string_pretty(&out).unwrap());
    assert_eq!(out["ok"], json!(true));
    let verdict = out["types"]["verdict"].as_str().unwrap().to_string();
    assert!(verdict == "equal" || verdict == "different", "{verdict}");
    assert_eq!(types::override_active(&tree.project()), verdict == "different");

    // the template builds as is
    let out = call(&server, json!({"action": "build", "waitMs": 240_000})).await.expect("build");
    println!("build: {}", serde_json::to_string_pretty(&out).unwrap());
    assert_eq!(out["ok"], json!(true));
    let js = read(tree.addon().join("maps/scripts/main.js"));
    assert!(js.starts_with("// GENERATED") && js.contains("[main] loaded"), "{js}");
    assert!(!js.contains("cs_script/point_script\";\n") || js.contains("import"), "the game module stays external");
    // nothing but the .js lands in maps/scripts
    let stray: Vec<_> = std::fs::read_dir(tree.addon().join("maps/scripts")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(stray, ["main.js"], "rollup/tsc left files behind: {stray:?}");

    // a type error comes back parsed
    call(&server, json!({"action": "write", "name": "main", "content": "import { Instance } from \"cs_script/point_script\";\nInstance.NoSuchThing();\nconst n: number = \"x\";\n"})).await.unwrap();
    let out = call(&server, json!({"action": "build"})).await.expect("build");
    println!("build with errors: {}", serde_json::to_string_pretty(&out).unwrap());
    assert_eq!(out["ok"], json!(false));
    let errors = out["failed"][0]["errors"].as_array().unwrap();
    assert!(errors.len() >= 2, "{errors:?}");
    assert_eq!(errors[0]["file"], json!("src/main/index.ts"));
    assert_eq!(errors[0]["line"], json!(2));
    assert!(errors.iter().any(|e| e["code"] == json!("TS2322")));

    // the override wins: `Console` exists in the newer npm types only
    let probe = "import { Console } from \"cs_script/point_script\";\nConsole.Msg(\"x\");\n";
    call(&server, json!({"action": "write", "name": "main", "content": probe})).await.unwrap();
    let out = call(&server, json!({"action": "build"})).await.expect("build");
    println!("Console probe ({verdict}): ok={} {}", out["ok"], out["failed"]);
    if verdict == "different" {
        let on_game = out["failed"][0]["errors"].to_string();
        assert!(on_game.contains("Console") || on_game.contains("no exported member"), "{on_game}");
    }

    // update: same versions, same verdict, override unchanged
    let out = call(&server, json!({"action": "update", "waitMs": 240_000})).await.expect("update");
    println!("update: {}", serde_json::to_string_pretty(&out).unwrap());
    assert_eq!(out["types"]["verdict"], json!(verdict));
    let status = call(&server, json!({"action": "status"})).await.unwrap();
    println!("status: {}", serde_json::to_string_pretty(&status).unwrap());
}

/// The fingerprint script itself on two small declarations (real node + the project's typescript, so
/// it runs after a real init; here against a throwaway project under the target dir).
#[tokio::test]
#[ignore = "needs npm, node and the network; cargo test -p hammer-mcp --test cs_script -- --ignored --nocapture"]
async fn real_fingerprint_tells_api_changes_from_formatting() {
    let _env = ENV.lock().await;
    std::env::remove_var("CS_SCRIPT_NPM");
    std::env::remove_var("CS_SCRIPT_NODE");
    let tree = Tree::new("fingerprint");
    let dir = tree.root.join("fp");
    std::fs::create_dir_all(&dir).unwrap();
    // its own package.json: npm would otherwise adopt one from a parent folder
    std::fs::write(dir.join("package.json"), "{\"name\":\"fp\",\"private\":true}").unwrap();
    let npm = std::process::Command::new("npm")
        .args(["install", "--no-audit", "--no-fund", "typescript@5"])
        .current_dir(&dir)
        .output()
        .expect("npm");
    assert!(npm.status.success(), "{}", String::from_utf8_lossy(&npm.stderr));
    let a = dir.join("a.d.ts");
    let b = dir.join("b.d.ts");
    let c = dir.join("c.d.ts");
    std::fs::write(&a, "declare module \"cs_script/point_script\"\n{\n    export const Instance: Domain;\n    export interface Domain { Msg(s: string): void; Get(): number }\n    export enum E { A = 1, B = 2 }\n}\n").unwrap();
    // same API, different spelling and member order
    std::fs::write(&b, "declare module \"cs_script/point_script\" {\n    const Instance: Domain;\n    interface Domain {\n        Get(): number;\n        Msg(s: string): void;\n    }\n    enum E { A = 1, B = 2 }\n}\n").unwrap();
    // a method gone, one added
    std::fs::write(&c, "declare module \"cs_script/point_script\" {\n    const Instance: Domain;\n    interface Domain { Msg(s: string): void; Get(): string; Extra(): void }\n    enum E { A = 1, B = 3 }\n}\n").unwrap();
    let run = |x: &Path, y: &Path| {
        let out = std::process::Command::new("node")
            .args(["--input-type=module", "-e", cs_script::FINGERPRINT_JS, &x.display().to_string(), &y.display().to_string()])
            .current_dir(&dir)
            .output()
            .expect("node");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        types::verdict_from_report(&String::from_utf8_lossy(&out.stdout))
    };
    println!("a vs b: {:?}", run(&a, &b));
    println!("a vs c: {:?}", run(&a, &c));
    assert_eq!(run(&a, &b), types::Verdict::Equal, "formatting and member order are not API");
    assert!(matches!(run(&a, &c), types::Verdict::Different(_)), "a changed signature is");
}

// -------------------------------------------------------------------------------------------
// Review fixes: process groups, char boundaries, saved tsconfig keys, finished jobs
// -------------------------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_kills_the_whole_process_group_and_the_call_returns() {
    use hammer_mcp::cs_script::run;
    let tree = Tree::new("group");
    let script = tree.root.join("spawner.sh");
    let pidfile = tree.root.join("grandchild.pid");
    // the grandchild inherits stdout/stderr (the pipes), so a naive drain would wait for it
    std::fs::write(&script, format!("#!/bin/bash\nsleep 60 &\necho $! > '{}'\nwait\n", pidfile.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let started = std::time::Instant::now();
    let result = run::run(&script, &[], &tree.root, &[], std::time::Duration::from_millis(400)).await;
    assert!(result.timed_out, "{result:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(8), "returned in {:?}", started.elapsed());
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let alive = std::process::Command::new("kill").args(["-0", &pid.to_string()]).output().unwrap().status.success();
        // a killed child can linger as a zombie of its (dead) parent until init reaps it
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat")).map(|s| s.contains(") Z ")).unwrap_or(false);
        if !alive || zombie {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "grandchild {pid} survived the timeout");
        tokio::task::yield_now().await;
    }
}

#[test]
fn a_report_tail_cuts_on_a_char_boundary() {
    // 400 two-byte characters: byte 300 from the end lands inside one
    let text = format!("{}a", "é".repeat(400)); // 801 bytes: byte 501 is mid-character
    match types::verdict_from_report(&text) {
        types::Verdict::Unknown(why) => assert!(why.contains('é') && why.contains("...")),
        other => panic!("{other:?}"),
    }
    assert!(matches!(types::verdict_from_report(&format!("{{{text}")), types::Verdict::Unknown(_)));
}

#[test]
fn an_override_keeps_a_scripts_own_types_and_include_and_restores_them() {
    let tree = Tree::new("saved");
    let project = Project::new("mcp_test", &tree.addon());
    cs_script::scaffold(&project, &["mine".into(), "plain".into()]).unwrap();
    let dir = tree.project();
    let own = json!({
        "extends": "@s2ze/tsconfig",
        "compilerOptions": {"types": ["node"], "strict": true},
        "include": ["./**/*.ts"]
    });
    std::fs::write(dir.join("src/mine/tsconfig.json"), serde_json::to_string_pretty(&own).unwrap()).unwrap();
    let different = types::Verdict::Different(json!({}));
    types::sync_override(&dir, &different, Some(GAME_D_TS)).unwrap();
    let on: Value = serde_json::from_str(&read(dir.join("src/mine/tsconfig.json"))).unwrap();
    assert_eq!(on["compilerOptions"]["types"], json!([]));
    assert_eq!(on["compilerOptions"]["strict"], json!(true));
    // a refresh does not overwrite the saved originals with our values
    types::sync_override(&dir, &different, Some(GAME_D_TS)).unwrap();
    types::sync_override(&dir, &types::Verdict::Equal, Some(GAME_D_TS)).unwrap();
    let off: Value = serde_json::from_str(&read(dir.join("src/mine/tsconfig.json"))).unwrap();
    assert_eq!(off, own, "restored exactly");
    let plain: Value = serde_json::from_str(&read(dir.join("src/plain/tsconfig.json"))).unwrap();
    assert_eq!(plain, json!({"extends": "@s2ze/tsconfig"}));
    assert!(!dir.join(".cs-override-saved.json").exists(), "nothing left saved");
}

#[tokio::test]
async fn a_finished_job_nobody_joined_is_not_running_and_is_collected_once() {
    use hammer_mcp::cs_script::run;
    let tree = Tree::new("jobs");
    let key = format!("test:{}", tree.root.display());
    let first = run::run_job(&key, PathBuf::from("true"), vec![], tree.root.clone(), std::time::Duration::from_secs(30), std::time::Duration::ZERO).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while run::job_running(&key) {
        assert!(std::time::Instant::now() < deadline, "the job never ended");
        tokio::task::yield_now().await;
    }
    if first.is_some() {
        // finished within the zero wait and was collected already: nothing left to join
        assert!(!run::job_pending(&key));
        return;
    }
    assert!(!run::job_running(&key), "an ended job is not running");
    assert!(run::job_pending(&key), "its result waits for one collector");
    let got = run::run_job(&key, PathBuf::from("true"), vec![], tree.root.clone(), std::time::Duration::from_secs(30), std::time::Duration::from_secs(5)).await;
    assert!(got.expect("the finished result").ok());
    assert!(!run::job_pending(&key), "collected once, then gone");
}
