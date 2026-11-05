//! The real Valve files on this machine: read-only, nothing is copied into the repository and
//! no game binary is involved, so these run against a workshop install in place.
//!
//! Env-gated exactly like `crates/cs2-kv3/tests/real_files.rs`: the install is found through
//! `$CS2_MCP_PROJECT_ROOT` (default: this checkout), and without one the tests
//! print `skipped: sample missing …` and pass so `cargo test` stays green on a machine that has
//! no CS2 install.
//!
//! * [`every_gameinfo_and_vdf_parses_and_round_trips`] — every `gameinfo*.gi` and `*.vdf` under
//!   the install root. Each file has to parse with [`cs2_kv1::text::ReadOptions::default`] and
//!   `parse(write(parse(x)))` has to equal `parse(x)`; the per-file line says whether the bytes
//!   also came back unchanged.
//! * [`every_txt_under_game_csgo_reports_its_result`] — every `*.txt` under `<app>/game/csgo`.
//!   Not all of them are KV1: the KV3 ones (a `<!-- kv3 … -->` header), the plain name lists
//!   (one bare word per line — not KV1 at all) and the game's own live config (which the game
//!   rewrites while running, so it can be caught truncated) are classified by their content and
//!   pinned, instead of being skipped silently.

use std::fs;
use std::path::{Path, PathBuf};

use cs2_kv1::text;

/// The workshop install root, or `None` when this machine has none.
fn app_root() -> Option<PathBuf> {
    let project = std::env::var("CS2_MCP_PROJECT_ROOT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string());
    let app = Path::new(&project).join("workshop/run/app");
    app.is_dir().then_some(app)
}

/// Every file under `dir` whose name matches, sorted so the output is stable.
fn collect(dir: &Path, matches: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => collect(&path, matches, out),
            Ok(_) => {
                if path.file_name().and_then(|name| name.to_str()).is_some_and(matches) {
                    out.push(path);
                }
            }
            Err(_) => {}
        }
    }
    out.sort();
}

/// A path relative to `app`, for readable output.
fn relative(app: &Path, path: &Path) -> String {
    path.strip_prefix(app).unwrap_or(path).display().to_string()
}

fn read(app: &Path, path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", relative(app, path)))
}

/// Lines that are a quoted string at column 0 — one per root object of a KV1 document. Used to
/// confirm a multi-root file independently of the error the parser reports for it.
fn root_object_lines(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let line = line.trim_end_matches(['\r', ' ']);
            line.starts_with('"') && line.ends_with('"') && line.matches('"').count() == 2
        })
        .count()
}

/// A parse, a write and a re-parse that has to give the document back.
fn round_trip(app: &Path, path: &Path, text: &str) -> (bool, usize) {
    let name = relative(app, path);
    let doc = match text::parse(text) {
        Ok(doc) => doc,
        Err(err) => panic!("{name} should parse: {err:?}"),
    };

    let written = text::write(&doc);
    let again = text::parse(&written)
        .unwrap_or_else(|err| panic!("{name}: the written document does not parse: {err:?}"));
    assert_eq!(again, doc, "{name} does not round-trip");

    let pairs = doc.value.as_object().map(|object| object.entries.len()).unwrap_or(0);
    (written == text, pairs)
}

#[test]
fn every_gameinfo_and_vdf_parses_and_round_trips() {
    let Some(app) = app_root() else {
        println!("skipped: sample missing (no workshop install)");
        return;
    };

    let mut files = Vec::new();
    collect(
        &app,
        &|name| name.ends_with(".vdf") || (name.starts_with("gameinfo") && name.ends_with(".gi")),
        &mut files,
    );
    if files.is_empty() {
        println!("skipped: sample missing (no gameinfo or vdf under {})", app.display());
        return;
    }

    println!("{} gameinfo/*.vdf files under {}", files.len(), app.display());
    let (mut parsed, mut byte_identical, mut multi_root) = (0, 0, 0);

    for path in &files {
        let name = relative(&app, path);
        let bytes = read(&app, path);

        // Valve's installer scripts (`installscript.vdf`) are two documents in one file — a
        // signed `"installscript"`/`"InstallScript"` object followed by a `"kvsignatures"` one.
        // Upstream rejects a second root object and so does this port (see the reader's
        // `Found data after the root object` check), so these are the documented files of this
        // install that cannot be read.
        match text::parse(&bytes) {
            Ok(_) => {}
            Err(err) => {
                assert_eq!(
                    root_object_lines(&bytes),
                    2,
                    "{name}: only a two-root installer script may fail to parse, got {err:?}"
                );
                assert!(
                    format!("{err}").contains("multiple root objects"),
                    "{name}: expected the multiple-root rejection, got {err:?}"
                );
                assert!(
                    name.rsplit('/').next().is_some_and(|f| f.eq_ignore_ascii_case("installscript.vdf")),
                    "{name}: only installscript.vdf may be multi-root"
                );
                multi_root += 1;
                println!("multi-root {name}: {}", format!("{err}").lines().last().unwrap_or_default());
                continue;
            }
        }

        let (identical, pairs) = round_trip(&app, path, &bytes);
        parsed += 1;
        if identical {
            byte_identical += 1;
        }
        println!(
            "ok  {name} ({} bytes, {pairs} root pairs, {})",
            bytes.len(),
            if identical { "byte-identical" } else { "normalised by the writer" }
        );
    }

    println!(
        "summary: {} files — {parsed} parsed and round-tripped ({byte_identical} byte-identical), {multi_root} multi-root installer scripts",
        files.len()
    );
    assert_eq!(parsed + multi_root, files.len());
    // The game's own files: 6 `gameinfo*.gi` plus `game/csgo/ServerConfig.vdf`, `toolmanifest.vdf`
    // and 3 installer scripts (the multi-root ones), so at least 11 files of which at least 8
    // parse. A lower bound, since an install can only gain files. Two trees grow with use and are
    // parsed and round-tripped above but not counted: plugin vdfs under `addons/`
    // (`bridge/install-rs.sh` lays down `addons/metamod/cs2mcp.vdf` on every launch, a third-party
    // plugin may add more) and `game/csgo_addons/<addon>/` (every addon someone makes brings its
    // own `ServerConfig.vdf`). Their number is not the game's; the parse-everything assertion
    // above still covers them.
    let grows = |p: &PathBuf| {
        let rel = relative(&app, p);
        rel.split('/').any(|c| c == "addons") || rel.starts_with("game/csgo_addons/")
    };
    let user = files.iter().filter(|p| grows(p)).count();
    let user_parsed = files
        .iter()
        .filter(|p| grows(p) && text::parse(&read(&app, p)).is_ok())
        .count();
    assert!(
        files.len().saturating_sub(user) >= 11,
        "expected at least 6 gameinfo*.gi and 5 *.vdf (outside addons/ and game/csgo_addons/), found {}",
        files.len().saturating_sub(user)
    );
    assert!(
        parsed.saturating_sub(user_parsed) >= 8,
        "everything but the 3 installer scripts has to parse, only {} did",
        parsed.saturating_sub(user_parsed)
    );
}

#[test]
fn every_txt_under_game_csgo_reports_its_result() {
    let Some(app) = app_root() else {
        println!("skipped: sample missing (no workshop install)");
        return;
    };

    let csgo = app.join("game/csgo");
    if !csgo.is_dir() {
        println!("skipped: sample missing ({})", csgo.display());
        return;
    }

    let mut files = Vec::new();
    collect(&csgo, &|name| name.ends_with(".txt"), &mut files);
    if files.is_empty() {
        println!("skipped: sample missing (no .txt under {})", csgo.display());
        return;
    }

    println!("{} .txt files under {}", files.len(), csgo.display());
    let (mut parsed, mut kv3, mut name_list, mut live_config) = (0, 0, 0, 0);

    for path in &files {
        let name = relative(&app, path);
        let bytes = read(&app, path);

        // `<!-- kv3 … -->`: that header is KeyValues3, which is `cs2-kv3`'s format.
        if bytes.starts_with("<!-- kv3") {
            let err = text::parse(&bytes).expect_err("a KV3 document is not KV1 text");
            kv3 += 1;
            println!("kv3 {name}: rejected as expected: {}", format!("{err}").lines().last().unwrap_or_default());
            continue;
        }

        // Valve's import lists: a comment header and then one name per line (a name may contain
        // spaces), with no KV1 punctuation anywhere — not a KV1 document, and every one of them
        // has to be rejected (a lone name is a key without a value, two are a second root
        // object).
        let is_name_list = bytes
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('/'))
            .all(|line| !line.contains(['{', '}', '"', '=']));
        if is_name_list {
            let err = text::parse(&bytes).expect_err("a bare name list is not a KV1 document");
            name_list += 1;
            println!("list {name}: rejected as expected: {}", format!("{err}").lines().last().unwrap_or_default());
            continue;
        }

        // The game's own live configs under `cfg/` are rewritten by the game while it runs, so a
        // write can be caught half-done: treat them as data, not as a claim about the format.
        if path.parent().is_some_and(|parent| parent.ends_with("cfg")) {
            match text::parse(&bytes) {
                Ok(doc) => {
                    let written = text::write(&doc);
                    let again = text::parse(&written).expect("the written document parses");
                    assert_eq!(again, doc, "{name} does not round-trip");
                    parsed += 1;
                    println!("ok  {name} ({} bytes, live config)", bytes.len());
                }
                Err(err) => {
                    live_config += 1;
                    println!("incomplete {name}: {}", format!("{err}").lines().last().unwrap_or_default());
                }
            }
            continue;
        }

        let (identical, pairs) = round_trip(&app, path, &bytes);
        parsed += 1;
        println!(
            "ok  {name} ({} bytes, {pairs} root pairs, {})",
            bytes.len(),
            if identical { "byte-identical" } else { "normalised by the writer" }
        );
    }

    println!(
        "summary: {} files — {parsed} KV1 (parsed and round-tripped), {kv3} KV3, {name_list} name lists, {live_config} live configs caught mid-write",
        files.len()
    );
    assert_eq!(parsed + kv3 + name_list + live_config, files.len());
    // This install: 10 `backup_round*.txt` and 4 `import_scripts/source1import_*.txt` script
    // files, 4 KV3 files under `tools/`, 8 name lists and 1 live config under `cfg/`.
    assert_eq!(files.len(), 27);
    assert_eq!(parsed, 15 - live_config, "every non-cfg KV1 script file has to parse");
    assert!(parsed >= 14, "the 14 KV1 script files have to parse");
    assert_eq!(kv3, 4);
    assert_eq!(name_list, 8);
    assert!(live_config <= 1, "only cfg/cs2_video.txt may be caught mid-write");
}
