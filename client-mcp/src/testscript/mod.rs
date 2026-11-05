//! tier0 `CTestScriptMgr` test scripts (`-insecure -testscript <file>`), the engine's own
//! automation runner. Script language and rules: ../idabins/docs/interfaces/ITestScriptMgr.md,
//! ../idabins/worknotes/vtest/README.md. Fragments `*.vtest` via `Test_Run <name> args`.
//! Checkpoints we can wait on: map_load, client_connected, FinishedMapLoad, Wrote_Screenshot,
//! frame_end, client_disconnected, DemoPlaybackStarted/Finished; ours via bridge `test.checkpoint`.
//!
//! This module renders and stages suites. `judge` checks the evidence a finished run left behind;
//! `run` owns the background task and the run table behind `run_testscript` / `test_status`.
//! The embedded files under `assets/vtest/` are byte-identical copies of the POC (see the README
//! there).

pub mod judge;
pub mod run;

use anyhow::{bail, Context, Result};
use cs2_mcp_common::runtime::docker::valid_suite_name;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

/// Fragments every staged suite gets. run_vtest.sh copies all of `suites/` the same way.
pub const FRAGMENTS: &[(&str, &str)] = &[
    ("mark.vtest", include_str!("../../assets/vtest/mark.vtest")),
    (
        "setup_player.vtest",
        include_str!("../../assets/vtest/setup_player.vtest"),
    ),
    ("pos.vtest", include_str!("../../assets/vtest/pos.vtest")),
    ("snap.vtest", include_str!("../../assets/vtest/snap.vtest")),
];

/// Suites shipped with the server (`run_testscript {suite}`). `harness` and `sandbox` park until a
/// `vtest_release` checkpoint arrives, so as one-shot runs they end in a timeout.
pub const LIBRARY: &[(&str, &str)] = &[
    (
        "move_jump",
        include_str!("../../assets/vtest/move_jump.txt"),
    ),
    ("sandbox", include_str!("../../assets/vtest/sandbox.txt")),
    ("harness", include_str!("../../assets/vtest/harness.txt")),
];

/// Ends every rendered suite. `quit` flushes console.log; `Test_ExitProcess` is only the hang guard.
pub const TRAILER: [&str; 3] = ["quit", "Test_Wait 30", "Test_ExitProcess 3"];
/// Exit code of the hang guard: the process was still alive after `quit`.
pub const HANG_GUARD_EXIT: i64 = 3;

/// A test script plus the fragments staged next to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suite {
    /// File stem under `<app>/vtest/`: `[A-Za-z0-9_-]+`.
    pub name: String,
    /// The script as written. Library suites hold the POC file verbatim.
    pub body: String,
    /// Fragment file name (with `.vtest`) and its content, staged verbatim.
    pub fragments: Vec<(String, String)>,
}

impl Suite {
    pub fn library(name: &str) -> Option<Suite> {
        LIBRARY
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(n, body)| Suite {
                name: (*n).to_string(),
                body: (*body).to_string(),
                fragments: fragments(),
            })
    }

    pub fn inline(name: &str, body: &str) -> Suite {
        Suite {
            name: name.to_string(),
            body: body.to_string(),
            fragments: fragments(),
        }
    }

    /// The staged script text. Rules (ITestScriptMgr.md "Preprocessing" and "Specials"):
    /// * a line is split on `\r`, `\n` and `;`; `//` runs from its `//` to the end of the line and
    ///   is removed before the `;` split (whole-line comments are kept verbatim);
    /// * specials are case-insensitive (`TEST_WAIT`, `Test_Run`, `Mark` all work);
    /// * `mark <name>` expands to `Test_Run mark <name>` (checkpoint + wait barrier, so the marker
    ///   is logged and every earlier command has run);
    /// * a bare `Test_Checkpoint` is rejected (dropped when it lands during `Test_Wait`);
    /// * `Test_Run <frag>` must name a staged fragment (the engine lowercases the path);
    /// * `quit`, `exit` and `Test_ExitProcess` are rejected anywhere but the trailer: they end the
    ///   process before console.log is flushed. The script ends with [`TRAILER`]; a body that already
    ///   ends with it is not doubled, so rendering is idempotent (the POC move_jump.txt renders to
    ///   itself).
    pub fn render(&self) -> Result<String> {
        let mut out: Vec<String> = Vec::new();
        // CRLF and bare CR are plain line ends
        let normalized = self.body.replace("\r\n", "\n").replace('\r', "\n");
        for raw in normalized.split('\n') {
            let t = raw.trim();
            if t.is_empty() {
                out.push(String::new());
            } else if t.starts_with("//") {
                out.push(raw.to_string());
            } else {
                let code = match t.find("//") {
                    Some(i) => &t[..i],
                    None => t,
                };
                for piece in code.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                    out.push(self.command(piece)?);
                }
            }
        }
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        let n = out.len();
        if n >= TRAILER.len()
            && out[n - TRAILER.len()..]
                .iter()
                .zip(TRAILER)
                .all(|(line, t)| line.eq_ignore_ascii_case(t))
        {
            out.truncate(n - TRAILER.len());
        }
        for line in &out {
            let first = line.split_whitespace().next().unwrap_or("");
            if ENDS_PROCESS.iter().any(|w| first.eq_ignore_ascii_case(w)) {
                bail!("`{line}`: ends the process before console.log is flushed; the renderer appends the trailer (quit)");
            }
        }
        let mut text = out.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&TRAILER.join("\n"));
        text.push('\n');
        Ok(text)
    }

    fn command(&self, piece: &str) -> Result<String> {
        let words: Vec<&str> = piece.split_whitespace().collect();
        let Some(first) = words.first().copied() else {
            return Ok(piece.to_string());
        };
        if first.eq_ignore_ascii_case("mark") {
            return match words.as_slice() {
                [_, name]
                    if !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
                {
                    Ok(format!("Test_Run mark {name}"))
                }
                _ => bail!("`{piece}`: use `mark <name>` with a name of [A-Za-z0-9_]+"),
            };
        }
        if first.eq_ignore_ascii_case("Test_Checkpoint") {
            bail!("`{piece}`: a bare Test_Checkpoint is dropped when it lands during Test_Wait; use `mark <name>`");
        }
        if first.eq_ignore_ascii_case("Test_Run") {
            if let Some(frag) = words.get(1) {
                let file = format!("{}.vtest", frag.to_ascii_lowercase());
                if !self
                    .fragments
                    .iter()
                    .any(|(f, _)| f.to_ascii_lowercase() == file)
                {
                    let staged: Vec<&str> =
                        self.fragments.iter().map(|(f, _)| f.as_str()).collect();
                    bail!(
                        "`{piece}`: fragment {file} is not staged (have: {})",
                        staged.join(", ")
                    );
                }
            }
        }
        Ok(piece.to_string())
    }
}

/// Commands that end the process (checked case-insensitively, anywhere but the trailer).
const ENDS_PROCESS: [&str; 3] = ["quit", "exit", "Test_ExitProcess"];

/// Options for a generated per-map harness (`launch_session {map}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessOpts {
    /// a [`valid_map_name`], e.g. `de_dust2` or `editor/zoo/smartprop_zoo`
    pub map: String,
    /// SourceTV on before `map`, so `demo_record_start` works on any test map. Default true.
    pub tv: bool,
    /// a [`valid_addon`] name: the map loads as `map_workshop <addon> <map>` (the addon's own map
    /// and its workshop name), instead of `map <map>`. `None`: plain `map`.
    pub addon: Option<String>,
}

/// The per-map harness: the POC `harness.txt` for `opts.map`, with the `VTEST_MAP_LOADED` mark after
/// the map-load wait. Named [`harness_name`]. The map must already pass [`valid_map_name`]: it is
/// written into the script as a command.
pub fn harness_suite(opts: &HarnessOpts) -> Suite {
    debug_assert!(valid_map_name(&opts.map), "{:?}", opts.map);
    debug_assert!(opts.addon.as_deref().is_none_or(valid_addon));
    let mut lines = vec![
        "// harness: boot once for an in-session gametest run (worknotes/vtest/gametest/).".to_string(),
        "// Loads the map, spawns the player, then parks; the host runner drives every test over the bridge".to_string(),
        "// (input.*, console.exec, test.checkpoint, game.tick) and releases with `test.checkpoint vtest_release`.".to_string(),
        "testscript_debug 1".to_string(),
        "sv_cheats 1".to_string(),
        "r_always_render_all_windows 1".to_string(),
    ];
    if opts.tv {
        lines.push("tv_enable 1".to_string());
        lines.push("tv_delay 0".to_string());
    }
    lines.push(match &opts.addon {
        Some(addon) => format!("map_workshop {addon} {}", opts.map),
        None => format!("map {}", opts.map),
    });
    lines.push("Test_WaitForCheckPoint FinishedMapLoad once".to_string());
    lines.push("Test_Run mark VTEST_MAP_LOADED".to_string());
    lines.push("Test_Run setup_player".to_string());
    lines.push("Test_Run mark VTEST_PARKED".to_string());
    lines.push("Test_WaitForCheckPoint vtest_release".to_string());
    Suite {
        name: harness_name(&opts.map, opts.addon.as_deref()),
        body: lines.join("\n") + "\n",
        fragments: fragments(),
    }
}

/// Suite (file stem) of the per-map harness: `harness-` plus the map with each `/` as `--`, with
/// `<addon>--` in front when an addon loads it. `editor/zoo/smartprop_zoo` gives
/// `harness-editor--zoo--smartprop_zoo`; `cs_script_demo` with addon `cs_script_demo` gives
/// `harness-cs_script_demo--cs_script_demo`. Unambiguous: [`valid_map_name`] refuses `--` in a map
/// and [`valid_addon`] refuses `-`, so each `--` is one separator. `a/b` (`harness-a--b`) and `a-b`
/// (`harness-a-b`) cannot collide.
pub fn harness_name(map: &str, addon: Option<&str>) -> String {
    let encoded = map.replace('/', "--");
    match addon {
        Some(addon) => format!("harness-{addon}--{encoded}"),
        None => format!("harness-{encoded}"),
    }
}

/// An addon name for `-addon` and `map_workshop`: `[A-Za-z0-9_]{1,64}`.
pub fn valid_addon(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

static MAP_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9_\-]*(/[A-Za-z0-9_][A-Za-z0-9_\-]*)*$").expect("regex")
});

/// A map name the engine's `map` command takes and a suite file name can carry: `/`-separated
/// segments of `[A-Za-z0-9_-]`, each starting with `[A-Za-z0-9_]`, at most 128 bytes. No `..`, no
/// leading `/`, no backslash, no `//`, no `--` (the harness name encodes `/` as `--`).
pub fn valid_map_name(s: &str) -> bool {
    s.len() <= 128
        && !s.contains("--")
        && !s.contains("..")
        && !s.starts_with('/')
        && !s.contains('\\')
        && !s.contains("//")
        && MAP_NAME.is_match(s)
}

fn fragments() -> Vec<(String, String)> {
    FRAGMENTS
        .iter()
        .map(|(n, c)| ((*n).to_string(), (*c).to_string()))
        .collect()
}

/// Write `<app_root>/vtest/<name>.txt` (the rendered script) and every fragment verbatim, like
/// run_vtest.sh stages `suites/`. Returns the host path of the script. No `-testmode`.
///
/// A file is rewritten only when its content differs, through a temp file and a rename, so a
/// suite that is already staged (possibly in use by a running session) is never touched.
pub fn stage(app_root: &Path, suite: &Suite) -> Result<PathBuf> {
    if !valid_suite_name(&suite.name) {
        bail!("suite name {:?} must match [A-Za-z0-9_-]+", suite.name);
    }
    let script = suite.render()?;
    let dir = app_root.join("vtest");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    for (file, content) in &suite.fragments {
        write_if_changed(&dir.join(file), content)?;
    }
    let path = dir.join(format!("{}.txt", suite.name));
    write_if_changed(&path, &script)?;
    Ok(path)
}

/// Remove a staged script (used when a run is refused after staging an inline suite).
pub fn unstage(staged: &Path) {
    let _ = std::fs::remove_file(staged);
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // unique per call: two first-time stagings of the same file must not share a temp name
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}-{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_app(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("ts-stage-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn move_jump_renders_to_the_poc_file_byte_for_byte() {
        let suite = Suite::library("move_jump").unwrap();
        let text = suite.render().unwrap();
        assert_eq!(text, include_str!("../../assets/vtest/move_jump.txt"));
        // idempotent: a rendered script renders to itself
        let again = Suite::inline("move_jump", &text).render().unwrap();
        assert_eq!(again, text);
    }

    #[test]
    fn every_library_suite_ends_with_the_trailer_once() {
        for (name, _) in LIBRARY {
            let text = Suite::library(name).unwrap().render().unwrap();
            assert!(
                text.ends_with("quit\nTest_Wait 30\nTest_ExitProcess 3\n"),
                "{name}"
            );
            assert!(
                !text.contains("quit\nTest_Wait 30\nTest_ExitProcess 3\nquit"),
                "{name}"
            );
        }
    }

    #[test]
    fn inline_golden_splits_semicolons_and_expands_markers() {
        let body = "// header; kept verbatim\ntestscript_debug 1\nmap de_dust2; mark VTEST_BEGIN\n\nTest_WaitForCheckPoint FinishedMapLoad once\nTest_Run pos spawn\n";
        let text = Suite::inline("inline-golden", body).render().unwrap();
        assert_eq!(
            text,
            "// header; kept verbatim\ntestscript_debug 1\nmap de_dust2\nTest_Run mark VTEST_BEGIN\n\nTest_WaitForCheckPoint FinishedMapLoad once\nTest_Run pos spawn\nquit\nTest_Wait 30\nTest_ExitProcess 3\n"
        );
    }

    #[test]
    fn empty_body_is_just_the_trailer() {
        let text = Suite::inline("empty", "").render().unwrap();
        assert_eq!(text, "quit\nTest_Wait 30\nTest_ExitProcess 3\n");
    }

    #[test]
    fn rejects_bare_checkpoints_mid_script_quit_and_unknown_fragments() {
        let bare = Suite::inline("bad", "Test_Checkpoint x\n")
            .render()
            .unwrap_err();
        assert!(bare.to_string().contains("mark <name>"), "{bare}");
        let mid = Suite::inline("bad", "quit\nmap de_dust2\n")
            .render()
            .unwrap_err();
        assert!(mid.to_string().contains("trailer"), "{mid}");
        let frag = Suite::inline("bad", "Test_Run nope VTEST_X\n")
            .render()
            .unwrap_err();
        assert!(frag.to_string().contains("nope.vtest"), "{frag}");
        assert!(Suite::inline("bad", "mark bad-name\n").render().is_err());
    }

    #[test]
    fn stage_writes_the_script_and_fragments_verbatim() {
        let app = temp_app("ok");
        let suite = Suite::library("move_jump").unwrap();
        let path = stage(&app, &suite).unwrap();
        assert_eq!(path, app.join("vtest/move_jump.txt"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            include_str!("../../assets/vtest/move_jump.txt")
        );
        for (name, content) in FRAGMENTS {
            assert_eq!(
                std::fs::read_to_string(app.join("vtest").join(name)).unwrap(),
                *content
            );
        }
        let _ = std::fs::remove_dir_all(&app);
    }

    #[test]
    fn stage_refuses_names_that_are_not_file_stems() {
        let app = temp_app("badname");
        let mut suite = Suite::inline("inline", "");
        suite.name = "../escape".into();
        assert!(stage(&app, &suite).is_err());
        assert!(!app.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(&app);
    }

    #[test]
    fn specials_are_case_insensitive() {
        let text = Suite::inline("case", "TEST_WAIT 1\nMark MARK_A\ntest_run MARK\n")
            .render()
            .unwrap();
        assert_eq!(
            text,
            "TEST_WAIT 1\nTest_Run mark MARK_A\ntest_run MARK\nquit\nTest_Wait 30\nTest_ExitProcess 3\n"
        );
        // mixed-case refusals too
        assert!(Suite::inline("c", "TEST_CHECKPOINT x\n").render().is_err());
        assert!(Suite::inline("c", "Test_Run NOPE\n").render().is_err());
        assert!(
            Suite::inline("c", "Test_Run MARK\n").render().is_ok(),
            "fragment lookup is case-insensitive"
        );
        assert!(Suite::inline("c", "EXIT\n").render().is_err());
        assert!(Suite::inline("c", "Quit\n").render().is_err());
    }

    #[test]
    fn process_ending_specials_are_refused_mid_script() {
        let err = Suite::inline("x", "map de_dust2; test_exitprocess 0\n")
            .render()
            .unwrap_err();
        assert!(err.to_string().contains("trailer"), "{err}");
        assert!(Suite::inline("x", "map de_dust2; exit\n").render().is_err());
    }

    #[test]
    fn comments_are_stripped_before_the_semicolon_split() {
        // "//" runs to the end of the line, so the ';' after it is part of the comment
        let body =
            "map de_dust2 // load; then wait\nTest_Wait 1; // trailing\n// whole line; kept\n";
        let text = Suite::inline("c", body).render().unwrap();
        assert_eq!(
            text,
            "map de_dust2\nTest_Wait 1\n// whole line; kept\nquit\nTest_Wait 30\nTest_ExitProcess 3\n"
        );
    }

    #[test]
    fn crlf_bodies_split_like_lf_bodies() {
        let lf = Suite::inline("c", "map a\nTest_Wait 1\n").render().unwrap();
        let crlf = Suite::inline("c", "map a\r\nTest_Wait 1\r\n")
            .render()
            .unwrap();
        assert_eq!(lf, crlf);
    }

    #[test]
    fn de_dust2_harness_is_the_poc_harness_plus_the_map_loaded_mark() {
        let opts = HarnessOpts {
            map: "de_dust2".into(),
            tv: true,
            addon: None,
        };
        let suite = harness_suite(&opts);
        assert_eq!(suite.name, "harness-de_dust2");
        let text = suite.render().unwrap();
        let poc = include_str!("../../assets/vtest/harness.txt");
        let expected = poc.replacen(
            "Test_WaitForCheckPoint FinishedMapLoad once\n",
            "Test_WaitForCheckPoint FinishedMapLoad once\nTest_Run mark VTEST_MAP_LOADED\n",
            1,
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn harness_without_tv_drops_only_the_tv_lines() {
        let opts = HarnessOpts {
            map: "editor/zoo/smartprop_zoo".into(),
            tv: false,
            addon: None,
        };
        let text = harness_suite(&opts).render().unwrap();
        assert!(!text.contains("tv_enable") && !text.contains("tv_delay"));
        assert!(text.contains("\nmap editor/zoo/smartprop_zoo\n"));
        assert!(text.ends_with("quit\nTest_Wait 30\nTest_ExitProcess 3\n"));
    }

    #[test]
    fn harness_names_encode_slashes_and_stay_suite_names() {
        assert_eq!(
            harness_name("editor/zoo/smartprop_zoo", None),
            "harness-editor--zoo--smartprop_zoo"
        );
        assert_eq!(harness_name("de_dust2", None), "harness-de_dust2");
        assert!(valid_suite_name(&harness_name(
            "editor/zoo/smartprop_zoo",
            None
        )));
    }

    #[test]
    fn addon_harness_loads_through_map_workshop_and_has_its_own_name() {
        let opts = HarnessOpts {
            map: "cs_script_demo".into(),
            tv: true,
            addon: Some("cs_script_demo".into()),
        };
        let suite = harness_suite(&opts);
        assert_eq!(suite.name, "harness-cs_script_demo--cs_script_demo");
        let text = suite.render().unwrap();
        assert!(
            text.contains("\nmap_workshop cs_script_demo cs_script_demo\n"),
            "{text}"
        );
        assert!(!text.contains("\nmap cs_script_demo\n"), "{text}");
        // the same map without the addon is another file
        let plain = harness_suite(&HarnessOpts {
            addon: None,
            ..opts.clone()
        });
        assert_eq!(plain.name, "harness-cs_script_demo");
        assert!(plain.render().unwrap().contains("\nmap cs_script_demo\n"));
        // the golden of the addon-free harness is untouched by the addon field
        let poc = include_str!("../../assets/vtest/harness.txt");
        let expected = poc.replacen(
            "Test_WaitForCheckPoint FinishedMapLoad once\n",
            "Test_WaitForCheckPoint FinishedMapLoad once\nTest_Run mark VTEST_MAP_LOADED\n",
            1,
        );
        assert_eq!(
            harness_suite(&HarnessOpts {
                map: "de_dust2".into(),
                tv: true,
                addon: None
            })
            .render()
            .unwrap(),
            expected
        );
    }

    #[test]
    fn addon_names_are_underscore_words_so_the_separator_stays_unique() {
        assert!(valid_addon("cs_script_demo"));
        assert!(valid_addon(&"a".repeat(64)));
        for bad in ["", "cs-script", "a b", "a.b", "x;y", &"a".repeat(65)] {
            assert!(!valid_addon(bad), "{bad:?}");
        }
        // an addon with `-` could forge the separator; the name rule refuses it
        assert!(!valid_addon("a--b"));
        assert!(!valid_addon("a-b"));
        // a map cannot carry the separator, so `harness-<addon>--<map>` has one reading
        assert!(!valid_map_name("a--b"));
        assert_eq!(harness_name("b", Some("a")), "harness-a--b");
    }

    #[test]
    fn a_slash_and_a_dash_never_name_the_same_harness() {
        assert_ne!(harness_name("a/b", None), harness_name("a-b", None));
        // the separator cannot appear inside a map, so the encoding has no second preimage
        assert!(!valid_map_name("a--b"));
        assert!(valid_map_name("a-/b"));
        assert_eq!(harness_name("a-/b", None), "harness-a---b");
    }

    #[test]
    fn concurrent_first_time_stagings_do_not_race_on_one_temp_name() {
        let app = temp_app("race");
        let suite = Suite::library("move_jump").unwrap();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        stage(&app, &suite)
                    })
                })
                .collect();
            for w in workers {
                w.join().unwrap().unwrap();
            }
        });
        let leftovers: Vec<_> = std::fs::read_dir(app.join("vtest"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&app);
    }

    #[test]
    fn map_names_accept_engine_paths_and_refuse_traversal_and_junk() {
        for ok in ["editor/zoo/smartprop_zoo", "de_dust2", "cs_script_demo"] {
            assert!(valid_map_name(ok), "{ok}");
        }
        let long = "a".repeat(128);
        assert!(valid_map_name(&long));
        for bad in [
            "../x",
            "a//b",
            "C:\\x",
            "/x",
            "a/../b",
            "a/",
            "a b",
            "a--b",
            "a;quit",
            "",
            &"a".repeat(129),
        ] {
            assert!(!valid_map_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn unchanged_fragments_are_not_rewritten() {
        let app = temp_app("nowrite");
        let suite = Suite::library("move_jump").unwrap();
        stage(&app, &suite).unwrap();
        let frag = app.join("vtest/mark.vtest");
        // age the file: a rewrite would give it a fresh mtime
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&frag)
            .unwrap()
            .set_modified(old)
            .unwrap();
        stage(&app, &suite).unwrap();
        let mtime = std::fs::metadata(&frag).unwrap().modified().unwrap();
        assert!(
            mtime <= old + std::time::Duration::from_secs(1),
            "fragment was rewritten"
        );
        assert!(!app.join("vtest/.mark.vtest.tmp").exists());
        let _ = std::fs::remove_dir_all(&app);
    }
}
