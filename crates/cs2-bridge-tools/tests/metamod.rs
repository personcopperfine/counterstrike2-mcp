//! `cargo bridge metamod` against a stub `podman` (`PODMAN=<stub>`) and a throwaway reference
//! clone with one submodule: the host half runs for real (the local clone, the pin, the
//! publish, `BUILDINFO.txt`), the containers do not. The stub records every podman argv, the
//! `XDG_RUNTIME_DIR` it saw and the script piped to `bash -s`, and plays the container's part
//! (a staged `addons/` tree and a `buildinfo-<platform>.txt` in the mounted `/deps`), so no
//! network, image or game is needed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The stub podman. `image exists` answers from a marker file the stub's `build` creates;
/// `run` saves its stdin, then (unless `STUB_RUN_FAIL` is set) writes what the container would
/// have left in the `/deps` mount.
const STUB: &str = r#"#!/bin/sh
{ printf 'XDG=%s\n' "$XDG_RUNTIME_DIR"; for a in "$@"; do printf '%s\n' "$a"; done; echo '@@end'; } >> "$STUB_DIR/argv.log"
case "$1 $2" in
"image exists") [ -f "$STUB_DIR/image-built" ]; exit $? ;;
"image inspect") echo "sha256:stub-image-id [stub@sha256:0]"; exit 0 ;;
esac
case "$1" in
build) touch "$STUB_DIR/image-built"; exit 0 ;;
run)
    cat > "$STUB_DIR/stdin.sh"
    [ -z "$STUB_RUN_FAIL" ] || exit 7
    deps=; prev=; platform=
    for a in "$@"; do
        if [ "$prev" = -v ]; then case "$a" in *:/deps) deps=${a%:/deps} ;; esac; fi
        prev=$a; platform=$a
    done
    if [ -n "$deps" ]; then
        bin="$deps/stage-$platform/addons/metamod/bin/linuxsteamrt64"
        mkdir -p "$bin"
        printf 'stub server %s\n' "$STUB_GENERATION" > "$bin/libserver.so"
        ln -sf libserver.so "$bin/server-link.so"
        echo "metamod_commit=stub" > "$deps/buildinfo-$platform.txt"
    fi
    exit 0 ;;
esac
exit 0
"#;

struct World {
    dir: tempfile::TempDir,
    mm: String,
    kh: String,
}

/// Git isolated from the user's config (no signing, no hooks, a fixed identity).
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(git_env())
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn git_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_AUTHOR_NAME", "test"),
        ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
        ("GIT_COMMITTER_NAME", "test"),
        ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
    ]
}

impl World {
    /// A reference clone `ref/` whose HEAD links `third_party/khook` (a second repo), the stub
    /// podman in `bin/`, and empty `deps/` / `out/` locations.
    fn new() -> World {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dir.path();
        let khook = root.join("khook-src");
        std::fs::create_dir_all(&khook).unwrap();
        git(&khook, &["init", "-q"]);
        std::fs::write(khook.join("khook.hpp"), "// khook\n").unwrap();
        git(&khook, &["add", "."]);
        git(&khook, &["commit", "-q", "-m", "khook"]);
        let kh = git(&khook, &["rev-parse", "HEAD"]);

        let reference = root.join("ref");
        std::fs::create_dir_all(&reference).unwrap();
        git(&reference, &["init", "-q"]);
        std::fs::write(reference.join("AMBuildScript"), "# mm\n").unwrap();
        git(&reference, &["add", "."]);
        git(&reference, &["commit", "-q", "-m", "mm"]);
        git(
            &reference,
            &["-c", "protocol.file.allow=always", "submodule", "add", "-q", khook.to_str().unwrap(), "third_party/khook"],
        );
        git(&reference, &["commit", "-q", "-m", "add khook"]);
        let mm = git(&reference, &["rev-parse", "HEAD"]);

        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("podman");
        std::fs::write(&stub, STUB).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        World { dir, mm, kh }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cs2-bridge-tools"));
        cmd.arg("metamod")
            .args(args)
            .envs(git_env())
            .env("PODMAN", self.path("bin/podman"))
            .env("STUB_DIR", self.dir.path())
            .env("MM_REF", self.path("ref"))
            .env("MM_DEPS", self.path("deps"))
            .env("MM_OUT", self.path("out"))
            .env("MM_COMMIT", &self.mm)
            .env("KH_COMMIT_EXPECTED", &self.kh)
            .env("XWIN", self.path("xwin"))
            .env("XDG_RUNTIME_DIR", self.path("xdg"))
            .env_remove("MM_NETWORK")
            .env_remove("MM_FORCE_IMAGE")
            .env_remove("MM_IMAGE")
            .env_remove("MM_BASE_IMAGE")
            .env_remove("STUB_RUN_FAIL");
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// The recorded podman calls, one argv each (the `XDG=` line first).
    fn calls(&self) -> Vec<Vec<String>> {
        let log = std::fs::read_to_string(self.path("argv.log")).unwrap_or_default();
        log.split("@@end\n")
            .filter(|c| !c.is_empty())
            .map(|c| c.lines().map(str::to_owned).collect())
            .collect()
    }

    fn reset_calls(&self) {
        let _ = std::fs::remove_file(self.path("argv.log"));
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn crate_file(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

#[test]
fn linux_clones_pins_builds_and_publishes() {
    let w = World::new();
    let out = w.run(&["linux"], &[("STUB_GENERATION", "1")]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(log.contains("src cold: cloning"), "{log}");
    assert!(log.contains("src cold: cloning submodule third_party/khook @"), "{log}");
    assert!(log.contains("src cold: cloned 2 git trees"), "{log}");
    assert!(log.contains("pinned src: metamod"), "{log}");

    // The source cache: pinned, with the submodule at its linked commit.
    let src = w.path("deps/src/metamod-source");
    assert_eq!(git(&src, &["rev-parse", "HEAD"]), w.mm);
    assert_eq!(git(&src.join("third_party/khook"), &["rev-parse", "HEAD"]), w.kh);

    // podman: the image is missing -> built; then the container run.
    let calls = w.calls();
    let xdg = format!("XDG={}", w.path("xdg").display());
    assert!(calls.iter().all(|c| c[0] == xdg), "{calls:?}");
    let argvs: Vec<Vec<&str>> = calls.iter().map(|c| c[1..].iter().map(String::as_str).collect()).collect();
    assert_eq!(argvs[0], ["image", "exists", "localhost/mmsource-build:debian11-clang22"]);
    let containerfile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/metamod/Containerfile");
    let containerfile = containerfile.canonicalize().unwrap();
    let context = w.path("deps/image-context");
    assert_eq!(
        argvs[1],
        [
            "build",
            "-t",
            "localhost/mmsource-build:debian11-clang22",
            "--build-arg",
            "BASE_IMAGE=ghcr.io/alliedmodders/build-containers/debian11-clang22:latest",
            "-f",
            containerfile.to_str().unwrap(),
            context.to_str().unwrap(),
        ]
    );
    assert!(context.is_dir() && std::fs::read_dir(&context).unwrap().next().is_none(), "an empty context");
    let run = &argvs[2];
    assert_eq!(&run[..4], ["run", "--rm", "-i", "--name"]);
    assert!(run[4].starts_with("mmsrc-linux-"), "{run:?}");
    let deps_mount = format!("{}:/deps", w.path("deps").display());
    assert_eq!(
        &run[5..],
        [
            "--label",
            "cs2mcp.metamod=linux",
            "--network",
            "host",
            "-v",
            deps_mount.as_str(),
            "localhost/mmsource-build:debian11-clang22",
            "bash",
            "-s",
            "--",
            "linux",
        ]
    );
    assert_eq!(std::fs::read_to_string(w.path("stdin.sh")).unwrap(), crate_file("src/metamod/container-build.sh"));

    // The published tree (symlinks stay symlinks) and BUILDINFO.txt.
    let bin = w.path("out/linux/addons/metamod/bin/linuxsteamrt64");
    assert_eq!(std::fs::read_to_string(bin.join("libserver.so")).unwrap(), "stub server 1\n");
    assert_eq!(std::fs::read_link(bin.join("server-link.so")).unwrap(), Path::new("libserver.so"));
    assert!(!w.path("out/linux/.addons.new").exists());
    let info = std::fs::read_to_string(w.path("out/BUILDINFO.txt")).unwrap();
    assert!(info.starts_with("Metamod:Source build info — 20"), "{info}");
    assert!(info.contains("## source\nmetamod_commit=stub\ns2sdk_pinned_by_upstream_lock="), "{info}");
    assert!(info.contains("build_image_id=sha256:stub-image-id [stub@sha256:0]"), "{info}");
    assert!(info.contains("containerfile_sha256="), "{info}");
    assert!(info.contains("## outputs: linux\nsha256 "), "{info}");
    assert!(info.contains("  addons/metamod/bin/linuxsteamrt64/libserver.so\nfile addons/metamod/bin/linuxsteamrt64/libserver.so: "), "{info}");
    assert!(!info.contains("server-link.so"), "find -type f lists no symlink: {info}");
    assert!(info.contains("## outputs: win64\nNOT BUILT — see the win64 STOP in crates/metamod/README.md"), "{info}");
    assert!(!w.path("out/.BUILDINFO.txt.tmp").exists());

    // Second run: both caches hit, the build re-runs, the output is replaced.
    w.reset_calls();
    let out = w.run(&["linux"], &[("STUB_GENERATION", "2")]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(log.contains("src cache hit"), "{log}");
    assert!(log.contains("image cache hit"), "{log}");
    let calls = w.calls();
    let verbs: Vec<&str> = calls.iter().map(|c| c[1].as_str()).collect();
    assert_eq!(verbs, ["image", "run", "image", "image"], "exists, run, then BUILDINFO's two inspects");
    assert_eq!(std::fs::read_to_string(bin.join("libserver.so")).unwrap(), "stub server 2\n");

    // MM_FORCE_IMAGE=1 rebuilds a present image.
    w.reset_calls();
    let out = w.run(&["image"], &[("MM_FORCE_IMAGE", "1")]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let calls = w.calls();
    assert_eq!(calls[0][1], "build", "{calls:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "sha256:stub-image-id [stub@sha256:0]");
}

#[test]
fn failures_leave_the_previous_output_untouched() {
    let w = World::new();
    assert_eq!(w.run(&["linux"], &[("STUB_GENERATION", "good")]).status.code(), Some(0));
    let lib = w.path("out/linux/addons/metamod/bin/linuxsteamrt64/libserver.so");
    let info = std::fs::read_to_string(w.path("out/BUILDINFO.txt")).unwrap();

    // A pin that is not a commit: refused before any podman call.
    w.reset_calls();
    let bogus = "0123456789abcdef0123456789abcdef01234567";
    let out = w.run(&["linux"], &[("MM_COMMIT", bogus), ("STUB_GENERATION", "bad")]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert!(log.contains(&format!("[metamod] FAIL: MM_COMMIT={bogus} is not a commit in the reference clone")), "{log}");
    assert!(w.calls().is_empty());

    // The container failing: nothing published.
    let out = w.run(&["linux"], &[("STUB_RUN_FAIL", "1"), ("STUB_GENERATION", "bad")]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert!(log.contains("[metamod] FAIL: linux build failed (container exited non-zero)"), "{log}");

    // A moved khook pin.
    let out = w.run(&["linux"], &[("KH_COMMIT_EXPECTED", bogus)]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert!(log.contains(&format!("expected {bogus} (the pin moved?)")), "{log}");

    assert_eq!(std::fs::read_to_string(&lib).unwrap(), "stub server good\n");
    assert_eq!(std::fs::read_to_string(w.path("out/BUILDINFO.txt")).unwrap(), info);
}

#[test]
fn a_dirty_source_cache_is_reset_to_the_pin() {
    let w = World::new();
    assert_eq!(w.run(&["linux"], &[]).status.code(), Some(0));
    let src = w.path("deps/src/metamod-source");
    std::fs::write(src.join("AMBuildScript"), "# patched by a win64 attempt\n").unwrap();
    std::fs::write(src.join("third_party/khook/khook.hpp"), "// patched\n").unwrap();
    let out = w.run(&["linux"], &[]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(log.contains("locally modified path(s) discarded"), "{log}");
    assert_eq!(std::fs::read_to_string(src.join("AMBuildScript")).unwrap(), "# mm\n");
    assert_eq!(std::fs::read_to_string(src.join("third_party/khook/khook.hpp")).unwrap(), "// khook\n");
}

#[test]
fn win64_stops_with_exit_3_and_the_blocker_list() {
    let w = World::new();
    let out = w.run(&["win64"], &[]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{log}");
    assert!(log.contains("[metamod] STOP: no win64 binaries."), "{log}");
    assert!(log.contains("[metamod]   cargo bridge metamod win64-attempt"), "{log}");
    assert!(!w.path("out/win64").exists());
    assert!(w.calls().is_empty(), "no container: {:?}", w.calls());

    // `all` builds linux first, then stops the same way.
    let out = w.run(&["all"], &[]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{log}");
    assert!(w.path("out/linux/addons").is_dir());
    assert!(!w.path("out/win64").exists());
}

#[test]
fn the_win64_containers_mount_the_sysroot() {
    let w = World::new();
    let out = w.run(&["win64-probe"], &[]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert!(log.contains("win64 needs the MSVC sysroot at"), "{log}");

    for d in ["xwin/crt", "xwin/sdk"] {
        std::fs::create_dir_all(w.path(d)).unwrap();
    }
    w.reset_calls();
    let out = w.run(&["win64-probe"], &[("MM_NETWORK", "slirp4netns")]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let calls = w.calls();
    let run = calls.iter().find(|c| c[1] == "run").expect("a run");
    let xwin_mount = format!("{}:/xwin:ro", w.path("xwin").display());
    assert!(run[5].starts_with("mmsrc-probe-"), "{run:?}");
    assert_eq!(
        &run[6..],
        ["--label", "cs2mcp.metamod=probe", "--network", "slirp4netns", "-v", xwin_mount.as_str(), "localhost/mmsource-build:debian11-clang22", "bash", "-s"]
    );
    assert_eq!(std::fs::read_to_string(w.path("stdin.sh")).unwrap(), crate_file("src/metamod/container-probe.sh"));

    w.reset_calls();
    let out = w.run(&["win64-attempt"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let calls = w.calls();
    let run = calls.iter().find(|c| c[1] == "run").expect("a run");
    let deps_mount = format!("{}:/deps", w.path("deps").display());
    assert!(run.contains(&deps_mount) && run.contains(&xwin_mount), "{run:?}");
    assert_eq!(run.last().map(String::as_str), Some("win64"));
    assert!(w.path("out/win64/addons").is_dir());
}

#[test]
fn a_missing_reference_clone_is_refused() {
    let w = World::new();
    let out = w.run(&["linux"], &[("MM_REF", w.path("nowhere").to_str().unwrap())]);
    let log = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert!(log.contains("reference clone missing:") && log.contains("(set MM_REF)"), "{log}");
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_cs2-bridge-tools")).args(["metamod", "win32"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
