//! The Linux session scripts (client-mcp/docker/): syntax, and the shared preamble run twice and
//! concurrently against a fake game tree. The preamble only touches files under the paths the test
//! sets (CS2_GAME_DIR, HOME, GSE_SAVE_DIR, CS2MCP_LOCK, CS2MCP_SHADER_DIR), never /etc or /app.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn docker_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/docker"))
}

fn scratch(name: &str) -> PathBuf {
    // CARGO_TARGET_TMPDIR is under the worktree's target dir, not /tmp
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn every_session_script_passes_bash_syntax_check() {
    for file in ["linux-client.sh", "linux-dedicated.sh", "lib-preamble.sh"] {
        let path = docker_dir().join(file);
        let out = Command::new("bash")
            .arg("-n")
            .arg(&path)
            .output()
            .expect("bash on PATH");
        assert!(
            out.status.success(),
            "bash -n {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A game tree in the state a previous run leaves behind: one file moved aside, one bundled file
/// to restore. Returns the game dir.
fn fake_game(root: &Path) -> PathBuf {
    let game = root.join("game");
    let bin = game.join("bin/linuxsteamrt64");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(game.join("csgo")).unwrap();
    std::fs::write(bin.join("libfoo.so.game-bundled"), "x").unwrap();
    std::fs::write(bin.join("libfreetype.so.6"), "x").unwrap();
    game
}

/// Run lock + fix + unlock, the way the Linux scripts do, with every path under `root`.
fn run_preamble(root: &Path) -> Output {
    let script = concat!(
        "set -u\n",
        "G=\"$CS2_GAME_DIR\"; B=\"$G/bin/linuxsteamrt64\"\n",
        "GSE_SAVE_DIR=\"$TEST_SAVE\"; OUT=\"$TEST_OUT\"\n",
        ". \"$LIB\"\n",
        "cs2mcp_lock\n",
        "cs2mcp_preamble_fix\n",
        "cs2mcp_unlock\n",
    );
    Command::new("bash")
        .arg("-c")
        .arg(script)
        .env("CS2_GAME_DIR", root.join("game"))
        .env("HOME", root.join("home"))
        .env("TEST_SAVE", root.join("save"))
        .env("TEST_OUT", root.join("out"))
        .env("CS2MCP_LOCK", root.join("lock/cs2mcp-preamble.lock"))
        .env("CS2MCP_SHADER_DIR", root.join("cache"))
        .env("LIB", docker_dir().join("lib-preamble.sh"))
        .output()
        .expect("bash on PATH")
}

fn assert_fixed_state(root: &Path) {
    let bin = root.join("game/bin/linuxsteamrt64");
    assert!(bin.join("libfoo.so").exists(), "bundled file restored");
    assert!(!bin.join("libfoo.so.game-bundled").exists());
    assert!(
        bin.join("libfreetype.so.6.game-bundled").exists(),
        "freetype moved aside"
    );
    assert!(!bin.join("libfreetype.so.6").exists());
    // dangling in this fake tree, so check the link itself, not its target
    assert!(
        bin.join("libfontconfig.so").symlink_metadata().is_ok(),
        "dev symlink"
    );
}

#[test]
fn the_preamble_is_repeatable_with_the_same_end_state() {
    let root = scratch("preamble-idempotent");
    fake_game(&root);
    let first = run_preamble(&root);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let out1 = String::from_utf8_lossy(&first.stdout);
    assert!(out1.contains("RESTORED libfoo.so"), "{out1}");
    assert!(out1.contains("MOVED-ASIDE libfreetype.so.6"), "{out1}");
    assert_fixed_state(&root);

    // a second run (a client after a server, or a restart) changes nothing: same files, same link
    // targets, same contents, and no RESTORED / MOVED-ASIDE / LINKED line
    let before = snapshot(&root);
    let second = run_preamble(&root);
    assert!(second.status.success());
    let out2 = String::from_utf8_lossy(&second.stdout);
    for word in ["RESTORED", "MOVED-ASIDE", "LINKED"] {
        assert!(!out2.contains(word), "second run printed {word}:\n{out2}");
    }
    assert!(out2.contains("DEV-SYMLINKS-CREATED"), "{out2}");
    assert!(!String::from_utf8_lossy(&second.stderr).contains("mv:"));
    assert_eq!(snapshot(&root), before, "second run changed the tree");
    assert_fixed_state(&root);
}

#[test]
fn a_second_run_does_not_restore_a_moved_aside_freetype_under_a_running_peer() {
    let root = scratch("preamble-peer");
    fake_game(&root);
    assert!(run_preamble(&root).status.success());
    // the bundled freetype is aside (as a running client needs it); another run must not undo that
    let bin = root.join("game/bin/linuxsteamrt64");
    assert!(bin.join("libfreetype.so.6.game-bundled").exists());
    let out = run_preamble(&root);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("RESTORED libfreetype"), "{text}");
    assert!(bin.join("libfreetype.so.6.game-bundled").exists());
    assert!(!bin.join("libfreetype.so.6").exists());
}

/// Every path under `root` with its kind, link target (for links) or contents (for files).
fn snapshot(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            let desc = if meta.file_type().is_symlink() {
                format!("link -> {}", std::fs::read_link(&p).unwrap().display())
            } else if meta.is_dir() {
                "dir".to_string()
            } else {
                format!("file {:?}", std::fs::read(&p).unwrap_or_default())
            };
            out.push((p.clone(), desc));
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

#[test]
fn concurrent_preambles_on_one_volume_serialise_and_agree() {
    let root = scratch("preamble-concurrent");
    fake_game(&root);
    let children: Vec<_> = (0..4)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || run_preamble(&root))
        })
        .collect();
    let outs: Vec<Output> = children.into_iter().map(|h| h.join().unwrap()).collect();
    for out in &outs {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        // a racing mv would print one of these; the lock makes them impossible
        assert!(!err.contains("No such file"), "{err}");
        assert!(!err.contains("cannot stat"), "{err}");
    }
    assert_fixed_state(&root);
    // every run either took the lock or said why it could not
    for out in &outs {
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("PREAMBLE-LOCK-HELD") || text.contains("PREAMBLE-LOCK-UNAVAILABLE"),
            "{text}"
        );
    }
}
