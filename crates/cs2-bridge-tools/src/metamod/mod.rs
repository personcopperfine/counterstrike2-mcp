//! `cargo bridge metamod <image|linux|win64|win64-probe|win64-attempt|all>`: build
//! Metamod:Source 2.0-dev (cs2 SDK only, x86_64) in a container, from the same metamod-source
//! commit our s2sdk headers pin against. `crates/metamod/README.md` is the operator's guide.
//!
//! ```text
//! linux          -> target/metamod/linux/addons
//! win64          -> STOP: AMBuild cannot cross-compile with clang-cl; prints the blocker list
//!                   (exit 3)
//! win64-probe    -> proves the win64 toolchain works (clang-cl + lld-link + the XWIN sysroot
//!                   link a real DLL)
//! win64-attempt  -> drives that toolchain through AMBuild with the two build-script
//!                   adaptations; fails at AMBuild's compiler probe (evidence)
//! image          -> build the AMBuild-carrying build image (crates/metamod/Containerfile)
//! all            -> image, then linux, then win64 (which stops)
//! ```
//!
//! Everything is cached under `target/metamod-deps/` (mounted at `/deps` in the container):
//!
//! * `src/metamod-source`: a local clone of the reference clone, pinned to `MM_COMMIT` (plus
//!   its submodules, recursively);
//! * `hl2sdk-cs2`: the cs2 SDK (alliedmodders/s2sdk, branch cs2), cloned by upstream's
//!   `checkout-deps.sh` inside the container;
//! * `build-<platform>`: the AMBuild build tree (so a rebuild is incremental);
//! * `stage-<platform>`: the packaged addons, handed to `target/metamod/` on success.
//!
//! Outputs are written to `target/metamod/<platform>/` only after the container exits 0, and
//! a bad pin fails before anything under `target/metamod/` is touched, so a failed run leaves
//! the previous output as it was. Every container runs with `--rm` and the label
//! `cs2mcp.metamod=<platform>` (`probe` for the probe).
//!
//! The host half (the source cache, the pin, the image, the publish, `BUILDINFO.txt`) is Rust;
//! the container half is the shell that runs inside the image, embedded here
//! (`container-build.sh`, `container-probe.sh`) and piped to `bash -s`.
//!
//! Environment overrides (an empty value is unset, as `${VAR:-default}` read it):
//!
//! | variable | default |
//! |---|---|
//! | `MM_COMMIT` | [`MM_COMMIT`] |
//! | `KH_COMMIT_EXPECTED` | [`KH_COMMIT`], the khook commit `MM_COMMIT` pins (checked, not fetched) |
//! | `MM_REF` | `<main checkout>/target/ext/metamod-source`, the read-only reference clone |
//! | `MM_DEPS` / `MM_OUT` | `<checkout>/target/metamod-deps` / `<checkout>/target/metamod` |
//! | `XWIN` | `<main checkout>/target/xwin`, the MSVC sysroot (crt + sdk) of the win64 runs |
//! | `MM_IMAGE` | [`IMAGE`] |
//! | `MM_BASE_IMAGE` | [`BASE_IMAGE`], passed to the image build as `--build-arg BASE_IMAGE` |
//! | `MM_FORCE_IMAGE=1` | rebuild the image even when the tag exists |
//! | `MM_NETWORK` | `host`, the containers' network mode (rootless pasta is broken on the development host; the mode does not affect the artefacts) |
//! | `PODMAN` | `podman` |
//! | `XDG_RUNTIME_DIR` | `<project root>/.podman/xdg` when unset (podman's rootless runtime dir) |
//!
//! A linked worktree reads the shared reference clone and sysroot from the main checkout
//! ([`crate::root::main_checkout`]) and keeps its caches and outputs in its own `target/`.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

use crate::root;

/// The command line of `cargo bridge metamod`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// What to build
    #[arg(value_enum)]
    pub what: What,
}

/// The subcommands of `cargo bridge metamod`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// Build the AMBuild-carrying build image
    Image,
    /// Build the Linux addons into target/metamod/linux/addons
    Linux,
    /// STOP: print the win64 blocker list and exit 3 (no win64 binaries)
    Win64,
    /// Prove the win64 toolchain (clang-cl + lld-link + the xwin sysroot) links a real DLL
    #[value(name = "win64-probe")]
    Win64Probe,
    /// Drive that toolchain through AMBuild with the build-script adaptations (fails: evidence)
    #[value(name = "win64-attempt")]
    Win64Attempt,
    /// image, then linux, then win64 (which stops)
    All,
}

/// Metamod:Source commit to build: 170058a "Bump khook with the latest changes" (2026-10-18).
pub const MM_COMMIT: &str = "170058a2f0dbd58106b7c76bfe88a94672295356";
/// The khook submodule commit [`MM_COMMIT`] pins.
pub const KH_COMMIT: &str = "0ca2b01363aff9fa11ad73b1b08dfdc2a195fb3a";
/// The image upstream CI builds the Linux half in (`.github/workflows/build-release.yml`).
pub const BASE_IMAGE: &str = "ghcr.io/alliedmodders/build-containers/debian11-clang22:latest";
/// The local tag of the derived build image.
pub const IMAGE: &str = "localhost/mmsource-build:debian11-clang22";

/// The container half of `linux` / `win64-attempt` (`bash -s -- <platform>`).
const CONTAINER_BUILD: &str = include_str!("container-build.sh");
/// The container half of `win64-probe` (`bash -s`).
const CONTAINER_PROBE: &str = include_str!("container-probe.sh");

/// Why win64 stops. Everything in it is measured, not guessed: `win64-probe` reproduces the
/// toolchain half and `win64-attempt` the AMBuild half.
const WIN64_BLOCKED: &str = "\
STOP: no win64 binaries. The blocker is AMBuild (alliedmodders/ambuild), not this
command, the image or the sysroot:
  1. no cross-compile target support. AMBuild's only knob is `target=` on DetectCxx
     (ambuild2/frontend/v2_2/cpp/detect.py:95-126); MM:S's AMBuildScript passes only
     target_arch, and there is no --target-os/--target_platform in `ambuild --help`.
     AMBuild's own windows cross path is mingw-only (`find_default_compiler`,
     detect.py:384-391 -> x86_64-w64-mingw32-*), whose C++ ABI is not the game's.
  2. clang-cl is always vendor \"clang\". The vendor comes from the probe program's macros
     (ambuild2/frontend/cpp/verify.py:190-212), and clang-cl defines __clang__, so
     AMBuildScript's `cxx.like('msvc')` branch (AMBuildScript:174) is unreachable and the
     gcc conventions apply: objectArgs/dependencyArgv (`-MD -MF dep`, gcc.py:66-69) which
     clang-cl ignores (`-MD` means /MD, `-MF` is \"unknown argument ignored\"), and
     `-m64` from the arch rules (cpp_rules.py:28), which is a gcc spelling.
  3. the SDK glue keys off the compiler family, not the target
     (hl2sdk-manifests/SdkHelpers.ambuild:229: non-msvc -> COMPILER_GCC, _LINUX, LINUX,
     POSIX), and picks protoc by target platform (SdkTarget.__init__ ->
     devtools/bin/protoc.exe, a PE binary that cannot run here without wine).
Making this work means patching three pinned upstream repositories (ambuild, MM:S,
hl2sdk-manifests) into a hand-rolled cross toolchain - i.e. a new upstream feature, well
past this task's budget, and it would no longer be the recipe upstream CI runs. The
win64 plugin should come from the GitHub Actions release artifact of the same commit
(built with MSVC on a Windows runner); that decision is the orchestrator's.
Evidence (reproducible):
  cargo bridge metamod win64-probe    toolchain is fine: clang-cl + lld-link + the xwin
                                      sysroot link a real DLL and llvm-readobj shows its
                                      export.
  cargo bridge metamod win64-attempt  the same build driven through AMBuild: fails at
                                      AMBuild's compiler probe (with the two build scripts
                                      patched) log: target/metamod-deps/win64-ambuild.log";

/// The exit code of the win64 STOP.
pub const WIN64_STOP: u8 = 3;

fn log(msg: impl AsRef<str>) {
    eprintln!("[metamod] {}", msg.as_ref());
}

/// Runs the command; `Ok` is the exit code (`3`: the win64 STOP). A failure is reported as one
/// `[metamod] FAIL: <reason>` line and exit 1.
pub fn run(args: Args) -> Result<u8> {
    let checkout = root::checkout();
    let main = root::main_checkout(&checkout);
    let cfg = Config::new(|k| std::env::var(k).ok(), &checkout, &main, root::project_root);
    match Metamod::new(cfg).run(args.what) {
        Ok(code) => Ok(code),
        Err(e) => {
            log(format!("FAIL: {e:#}"));
            Ok(1)
        }
    }
}

/// Everything the build reads from the environment and the checkout layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The checkout the outputs belong to (`UPSTREAM.lock`, `crates/metamod/Containerfile`).
    pub checkout: PathBuf,
    /// `MM_DEPS`: the dependency cache, mounted at `/deps`.
    pub deps: PathBuf,
    /// `MM_OUT`: the published outputs.
    pub out: PathBuf,
    /// `MM_REF`: the read-only reference clone the source cache is cloned from.
    pub reference: PathBuf,
    /// `MM_COMMIT`.
    pub mm_commit: String,
    /// `KH_COMMIT_EXPECTED`.
    pub kh_commit: String,
    /// `XWIN`: the MSVC sysroot of the win64 runs.
    pub xwin: PathBuf,
    /// `MM_BASE_IMAGE`.
    pub base_image: String,
    /// `MM_IMAGE`.
    pub image: String,
    /// `MM_FORCE_IMAGE=1`.
    pub force_image: bool,
    /// `MM_NETWORK`.
    pub network: String,
    /// `PODMAN`.
    pub podman: OsString,
    /// podman's `XDG_RUNTIME_DIR`.
    pub xdg_runtime_dir: PathBuf,
}

impl Config {
    /// The configuration from `env` (an empty value is unset), for `checkout` whose main
    /// checkout is `main`; `project_root` is only asked when `XDG_RUNTIME_DIR` is unset.
    pub fn new(
        env: impl Fn(&str) -> Option<String>,
        checkout: &Path,
        main: &Path,
        project_root: impl FnOnce() -> PathBuf,
    ) -> Config {
        let var = |k: &str| env(k).filter(|v| !v.is_empty());
        let path = |k: &str, default: PathBuf| var(k).map(PathBuf::from).unwrap_or(default);
        Config {
            checkout: checkout.to_path_buf(),
            deps: path("MM_DEPS", checkout.join("target/metamod-deps")),
            out: path("MM_OUT", checkout.join("target/metamod")),
            reference: path("MM_REF", main.join("target/ext/metamod-source")),
            mm_commit: var("MM_COMMIT").unwrap_or_else(|| MM_COMMIT.into()),
            kh_commit: var("KH_COMMIT_EXPECTED").unwrap_or_else(|| KH_COMMIT.into()),
            xwin: path("XWIN", main.join("target/xwin")),
            base_image: var("MM_BASE_IMAGE").unwrap_or_else(|| BASE_IMAGE.into()),
            image: var("MM_IMAGE").unwrap_or_else(|| IMAGE.into()),
            force_image: var("MM_FORCE_IMAGE").as_deref() == Some("1"),
            network: var("MM_NETWORK").unwrap_or_else(|| "host".into()),
            podman: var("PODMAN").map(OsString::from).unwrap_or_else(|| "podman".into()),
            xdg_runtime_dir: var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| root::podman_xdg(&project_root())),
        }
    }

    /// `crates/metamod/Containerfile`.
    pub fn containerfile(&self) -> PathBuf {
        self.checkout.join("crates/metamod/Containerfile")
    }

    /// `<deps>/src/metamod-source`, the pinned source cache.
    pub fn src(&self) -> PathBuf {
        self.deps.join("src/metamod-source")
    }

    /// The image build context: an empty directory. The Containerfile copies nothing, and its
    /// own directory (`crates/metamod/`) also holds the metamod-source submodule, which podman
    /// would otherwise send as the context.
    pub fn image_context(&self) -> PathBuf {
        self.deps.join("image-context")
    }

    /// `podman build` for the image.
    pub fn image_build_argv(&self) -> Vec<OsString> {
        argv([
            "build".as_ref(),
            "-t".as_ref(),
            self.image.as_ref(),
            "--build-arg".as_ref(),
            format!("BASE_IMAGE={}", self.base_image).as_ref(),
            "-f".as_ref(),
            self.containerfile().as_os_str(),
            self.image_context().as_os_str(),
        ])
    }

    /// `podman run` of the container build for `platform` (`linux` / `win64`), named after
    /// this process's `pid`; the script goes to its stdin.
    pub fn build_run_argv(&self, platform: &str, pid: u32) -> Vec<OsString> {
        let mut v = self.run_prefix(&format!("mmsrc-{platform}-{pid}"), platform);
        v.extend(argv(["-v".as_ref(), mount(&self.deps, "/deps").as_os_str()]));
        if platform == "win64" {
            v.extend(argv(["-v".as_ref(), mount(&self.xwin, "/xwin:ro").as_os_str()]));
        }
        v.extend(argv([self.image.as_ref(), "bash".as_ref(), "-s".as_ref(), "--".as_ref(), platform.as_ref()]));
        v
    }

    /// `podman run` of the win64 toolchain probe.
    pub fn probe_run_argv(&self, pid: u32) -> Vec<OsString> {
        let mut v = self.run_prefix(&format!("mmsrc-probe-{pid}"), "probe");
        v.extend(argv(["-v".as_ref(), mount(&self.xwin, "/xwin:ro").as_os_str(), self.image.as_ref(), "bash".as_ref(), "-s".as_ref()]));
        v
    }

    fn run_prefix(&self, name: &str, label: &str) -> Vec<OsString> {
        argv([
            "run".as_ref(),
            "--rm".as_ref(),
            "-i".as_ref(),
            "--name".as_ref(),
            name.as_ref(),
            "--label".as_ref(),
            format!("cs2mcp.metamod={label}").as_ref(),
            "--network".as_ref(),
            self.network.as_ref(),
        ])
    }
}

fn argv<'a>(items: impl IntoIterator<Item = &'a OsStr>) -> Vec<OsString> {
    items.into_iter().map(OsStr::to_os_string).collect()
}

/// `<host>:<container>` for `-v`.
fn mount(host: &Path, container: &str) -> OsString {
    let mut m = host.as_os_str().to_os_string();
    m.push(":");
    m.push(container);
    m
}

/// One run of the command over a [`Config`].
pub struct Metamod {
    cfg: Config,
}

impl Metamod {
    /// A run over `cfg`.
    pub fn new(cfg: Config) -> Self {
        Metamod { cfg }
    }

    /// Runs `what`; `Ok` is the exit code.
    pub fn run(&self, what: What) -> Result<u8> {
        match what {
            What::Linux => {
                self.sync_src()?;
                self.pin_src()?;
                self.ensure_image()?;
                self.container_build("linux").context("linux build failed (container exited non-zero)")?;
                self.publish("linux")?;
                Ok(0)
            }
            What::Win64 => {
                self.sync_src()?;
                self.pin_src()?;
                Ok(win64_blocked())
            }
            What::Win64Attempt => {
                self.sync_src()?;
                self.pin_src()?;
                self.ensure_image()?;
                self.container_build("win64")
                    .context("win64 cross build failed (see target/metamod-deps/win64-ambuild.log)")?;
                self.publish("win64")?;
                Ok(0)
            }
            What::Win64Probe => {
                self.ensure_image()?;
                self.container_probe()?;
                Ok(0)
            }
            What::Image => {
                self.ensure_image()?;
                let id = self.podman_output(&["image", "inspect", "--format", "{{.Id}} {{.RepoDigests}}", &self.cfg.image])?;
                println!("{id}");
                Ok(0)
            }
            What::All => {
                self.sync_src()?;
                self.pin_src()?;
                self.ensure_image()?;
                self.container_build("linux").context("linux build failed")?;
                self.publish("linux")?;
                Ok(win64_blocked())
            }
        }
    }

    // ------------------------------------------------------------ source cache

    /// A local clone of the reference clone (no network: the reference is a synced checkout
    /// with submodules). Submodule content is cloned out of the reference's
    /// `.git/modules/<path>` (recursively, for nested submodules such as khook -> safetyhook),
    /// so the reference stays read-only and the copy is self-contained.
    fn sync_src(&self) -> Result<()> {
        let src = self.cfg.src();
        if src.join(".git").is_dir() {
            log(format!("src cache hit: {} ({})", src.display(), git(&src, &["rev-parse", "--short", "HEAD"])?));
            return Ok(());
        }
        let reference = &self.cfg.reference;
        ensure!(reference.join(".git").is_dir(), "reference clone missing: {} (set MM_REF)", reference.display());
        log(format!("src cold: cloning {} -> {} (local clone, no network)", reference.display(), src.display()));
        let parent = src.parent().expect("src has a parent");
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        clone(reference, &src)?;
        let trees = 1 + clone_trees(&src, &reference.join(".git/modules"))?;
        log(format!("src cold: cloned {trees} git trees"));
        Ok(())
    }

    /// Resolves and checks out `MM_COMMIT`. Runs before anything under the outputs is
    /// touched, so a wrong pin fails without disturbing previous outputs.
    fn pin_src(&self) -> Result<()> {
        let src = self.cfg.src();
        let commit = &self.cfg.mm_commit;
        let spec = format!("{commit}^{{commit}}");
        ensure!(
            git_ok(&self.cfg.reference, &["rev-parse", "--verify", "--quiet", &spec]),
            "MM_COMMIT={commit} is not a commit in the reference clone {} (refusing to build)",
            self.cfg.reference.display()
        );
        ensure!(
            git_ok(&src, &["rev-parse", "--verify", "--quiet", &spec]),
            "MM_COMMIT={commit} is not in the source clone {} (refusing to build)",
            src.display()
        );
        git(&src, &["checkout", "--quiet", "--detach", "--force", commit])?;
        reset_trees(&src)?;
        self.verify_trees(&src)?;

        let khook = src.join("third_party/khook");
        let kh = git(&khook, &["rev-parse", "HEAD"])?;
        ensure!(kh == self.cfg.kh_commit, "khook is {kh}, expected {} (the pin moved?)", self.cfg.kh_commit);
        log(format!(
            "pinned src: metamod {}, khook {}",
            git(&src, &["rev-parse", "--short", "HEAD"])?,
            git(&khook, &["rev-parse", "--short", "HEAD"])?
        ));
        Ok(())
    }

    /// Every gitlink in `repo`'s HEAD is present at the pinned commit, recursively.
    fn verify_trees(&self, repo: &Path) -> Result<()> {
        let rerun = format!("remove {}/src and rerun", self.cfg.deps.display());
        for (path, sha) in gitlinks(repo)? {
            let sub = repo.join(&path);
            ensure!(sub.join(".git").is_dir(), "submodule {path} missing from {} ({rerun})", repo.display());
            let cur = git(&sub, &["rev-parse", "HEAD"])?;
            ensure!(cur == sha, "submodule {path} is at {cur}, expected {sha} ({rerun})");
            self.verify_trees(&sub)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ build image

    fn ensure_image(&self) -> Result<()> {
        let cfg = &self.cfg;
        let exists = || {
            self.podman()
                .args(["image", "exists", &cfg.image])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !cfg.force_image && exists() {
            log(format!("image cache hit: {}", cfg.image));
            return Ok(());
        }
        log(format!(
            "building {} from {} (base {})",
            cfg.image,
            cfg.containerfile().display(),
            cfg.base_image
        ));
        let context = cfg.image_context();
        std::fs::create_dir_all(&context).with_context(|| format!("creating {}", context.display()))?;
        let status = self
            .podman()
            .args(cfg.image_build_argv())
            .status()
            .with_context(|| format!("running {}", cfg.podman.to_string_lossy()))?;
        ensure!(status.success(), "podman build of {} failed ({status})", cfg.image);
        Ok(())
    }

    // ------------------------------------------------------------ containers

    /// The container build for `platform`, the dependency cache mounted at `/deps` (and the
    /// sysroot at `/xwin` for win64).
    fn container_build(&self, platform: &str) -> Result<()> {
        if platform == "win64" {
            self.need_xwin()?;
        }
        let argv = self.cfg.build_run_argv(platform, std::process::id());
        self.podman_with_stdin(&argv, CONTAINER_BUILD)
    }

    /// The win64 toolchain probe. Fails loudly (non-zero) if any piece is missing; it never
    /// invents a result.
    fn container_probe(&self) -> Result<()> {
        self.need_xwin()?;
        let argv = self.cfg.probe_run_argv(std::process::id());
        self.podman_with_stdin(&argv, CONTAINER_PROBE)
    }

    fn need_xwin(&self) -> Result<()> {
        let xwin = &self.cfg.xwin;
        ensure!(
            xwin.join("crt").is_dir() && xwin.join("sdk").is_dir(),
            "win64 needs the MSVC sysroot at {}/{{crt,sdk}} (set XWIN)",
            xwin.display()
        );
        Ok(())
    }

    fn podman(&self) -> Command {
        let mut cmd = Command::new(&self.cfg.podman);
        cmd.env("XDG_RUNTIME_DIR", &self.cfg.xdg_runtime_dir);
        cmd
    }

    /// Runs podman with `argv`, `script` on its stdin; `Err` unless it exits 0.
    fn podman_with_stdin(&self, argv: &[OsString], script: &str) -> Result<()> {
        let mut child = self
            .podman()
            .args(argv)
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", self.cfg.podman.to_string_lossy()))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        // A container that exits before reading all of it closes the pipe; its exit status is
        // the verdict then, not the write error.
        let _ = stdin.write_all(script.as_bytes());
        drop(stdin);
        let status = child.wait()?;
        ensure!(status.success(), "container exited non-zero ({status})");
        Ok(())
    }

    /// podman's stdout (trimmed); `Err` unless it exits 0.
    fn podman_output(&self, args: &[&str]) -> Result<String> {
        let out = self.podman().args(args).stderr(Stdio::inherit()).output()?;
        ensure!(out.status.success(), "podman {} failed ({})", args.join(" "), out.status);
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// podman's first stdout line, empty when it fails (a best-effort `BUILDINFO.txt` field).
    fn podman_field(&self, args: &[&str]) -> String {
        self.podman()
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_owned())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------ host-side output

    /// Hands `<deps>/stage-<platform>/addons` to `<out>/<platform>/addons`: copied next to the
    /// old output first and swapped in only once the copy is complete.
    fn publish(&self, platform: &str) -> Result<()> {
        let stage = self.cfg.deps.join(format!("stage-{platform}/addons"));
        ensure!(stage.is_dir(), "container left no package at {}", stage.display());
        let dir = self.cfg.out.join(platform);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let fresh = dir.join(".addons.new");
        remove_path(&fresh)?;
        copy_tree(&stage, &fresh)?;
        let addons = dir.join("addons");
        remove_path(&addons)?;
        std::fs::rename(&fresh, &addons).with_context(|| format!("moving {} into place", fresh.display()))?;
        log(format!("published {}", addons.display()));
        self.write_buildinfo(platform)
    }

    /// `<out>/BUILDINFO.txt`: the commits, the image, and a sha256 + `file` line for every
    /// published binary of both platforms. Written to a temp file and renamed.
    fn write_buildinfo(&self, platform: &str) -> Result<()> {
        let cfg = &self.cfg;
        let part = cfg.deps.join(format!("buildinfo-{platform}.txt"));
        ensure!(part.is_file(), "missing {} (the container did not run to completion)", part.display());
        let part = std::fs::read_to_string(&part).with_context(|| format!("reading {}", part.display()))?;
        let lock = cfg.checkout.join("crates/s2sdk/s2sdk-sys/UPSTREAM.lock");
        let lock = std::fs::read_to_string(&lock).with_context(|| format!("reading {}", lock.display()))?;

        let mut s = String::new();
        writeln!(s, "Metamod:Source build info — {}", utc_now())?;
        writeln!(s)?;
        writeln!(s, "## source")?;
        s.push_str(&part);
        if !part.ends_with('\n') && !part.is_empty() {
            s.push('\n');
        }
        writeln!(s, "s2sdk_pinned_by_upstream_lock={}", lock_commit(&lock))?;
        writeln!(s, "source_clone={}", cfg.src().display())?;
        writeln!(s)?;
        writeln!(s, "## image")?;
        writeln!(s, "base_image={}", cfg.base_image)?;
        writeln!(s, "base_image_digest={}", self.podman_field(&["image", "inspect", "--format", "{{.RepoDigests}}", &cfg.base_image]))?;
        writeln!(s, "build_image={}", cfg.image)?;
        writeln!(s, "build_image_id={}", self.podman_field(&["image", "inspect", "--format", "{{.Id}}", &cfg.image]))?;
        writeln!(s, "containerfile_sha256={}", sha256_file(&cfg.containerfile())?)?;
        let patch = cfg.deps.join("win64-buildsys.patch");
        if patch.is_file() {
            writeln!(s, "win64_buildsystem_patch_sha256={}", sha256_file(&patch)?)?;
            writeln!(
                s,
                "win64_buildsystem_patch_file={} (applied to the copy in target/metamod-deps/src; C++ sources untouched)",
                patch.display()
            )?;
        }
        writeln!(s)?;
        for p in ["linux", "win64"] {
            writeln!(s, "## outputs: {p}")?;
            let base = cfg.out.join(p);
            if base.join("addons").is_dir() {
                let mut files = Vec::new();
                list_files(&base.join("addons"), &mut files)?;
                let mut rels: Vec<String> = files
                    .iter()
                    .map(|f| f.strip_prefix(&base).expect("under base").to_string_lossy().into_owned())
                    .collect();
                rels.sort();
                for rel in rels {
                    let f = base.join(&rel);
                    writeln!(s, "sha256 {}  {rel}", sha256_file(&f)?)?;
                    writeln!(s, "file {rel}: {}", file_type(&f))?;
                }
            } else if p == "win64" {
                writeln!(s, "NOT BUILT — see the win64 STOP in crates/metamod/README.md: AMBuild has no")?;
                writeln!(s, "clang-cl/windows cross-compile support, so the plugin must come from the upstream")?;
                writeln!(s, "GitHub Actions win64 release artifact of the same commit (orchestrator's call).")?;
                writeln!(s, "Reproduce: cargo bridge metamod win64 (blocker list), win64-probe")?;
                writeln!(s, "(toolchain OK), win64-attempt (AMBuild failure log: target/metamod-deps/win64-ambuild.log).")?;
            } else {
                writeln!(s, "(not built)")?;
            }
            writeln!(s)?;
        }

        let out = cfg.out.join("BUILDINFO.txt");
        let tmp = cfg.out.join(".BUILDINFO.txt.tmp");
        std::fs::write(&tmp, s).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &out).with_context(|| format!("moving {} into place", out.display()))?;
        log(format!("wrote {}", out.display()));
        Ok(())
    }
}

/// Prints the win64 blocker list; returns [`WIN64_STOP`].
fn win64_blocked() -> u8 {
    for line in WIN64_BLOCKED.lines() {
        log(line);
    }
    WIN64_STOP
}

// ---------------------------------------------------------------- git

/// `git -C dir args`, stdout trimmed; `Err` (with git's stderr) unless it exits 0.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git -C {} {} failed ({}): {}",
            dir.display(),
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Whether `git -C dir args` exits 0 (its output discarded).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `git clone --quiet --no-hardlinks --no-checkout from to`.
fn clone(from: &Path, to: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone", "--quiet", "--no-hardlinks", "--no-checkout"])
        .arg(from)
        .arg(to)
        .status()
        .context("running git clone")?;
    ensure!(status.success(), "git clone {} {} failed ({status})", from.display(), to.display());
    Ok(())
}

/// The gitlinks (submodule `(path, commit)` pairs) of `repo`'s HEAD tree.
pub fn gitlinks(repo: &Path) -> Result<Vec<(String, String)>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-tree", "-r", "-z", "HEAD"])
        .output()
        .context("running git ls-tree")?;
    ensure!(
        out.status.success(),
        "git -C {} ls-tree -r HEAD failed: {}",
        repo.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(parse_gitlinks(&String::from_utf8_lossy(&out.stdout)))
}

/// The gitlinks of `git ls-tree -r -z` output: `<mode> <type> <sha>\t<path>` records, mode
/// `160000`.
pub fn parse_gitlinks(ls_tree: &str) -> Vec<(String, String)> {
    ls_tree
        .split('\0')
        .filter_map(|record| {
            let (meta, path) = record.split_once('\t')?;
            let mut meta = meta.split(' ');
            let (mode, _kind, sha) = (meta.next()?, meta.next()?, meta.next()?);
            (mode == "160000").then(|| (path.to_owned(), sha.to_owned()))
        })
        .collect()
}

/// For every gitlink in `dest`'s HEAD, clones the referenced tree out of `<store>/<path>` into
/// `<dest>/<path>` at the linked commit and recurses into its own `modules/`. Returns how many
/// trees were cloned.
fn clone_trees(dest: &Path, store: &Path) -> Result<usize> {
    let mut cloned = 0;
    for (path, sha) in gitlinks(dest)? {
        let from = store.join(&path);
        ensure!(
            from.is_dir(),
            "reference clone has no objects for submodule {path} ({})",
            from.display()
        );
        log(format!("src cold: cloning submodule {path} @ {}", git(&from, &["rev-parse", "--short", &sha])?));
        let to = dest.join(&path);
        remove_path(&to)?;
        clone(&from, &to)?;
        git(&to, &["checkout", "--quiet", "--detach", "--force", &sha])?;
        cloned += 1 + clone_trees(&to, &from.join("modules"))?;
    }
    Ok(cloned)
}

/// Forces every tree in the copy (and every nested clone) back to the revision it is checked
/// out at: the copy is a cache, so a local adaptation from an earlier run (the win64
/// experiment's build-script patch) must never leak into a later build. Logs what it discarded.
fn reset_trees(repo: &Path) -> Result<()> {
    let status = git(repo, &["status", "--porcelain", "--ignore-submodules=dirty"])?;
    let dirty = status.lines().filter(|l| !l.is_empty()).count();
    git(repo, &["checkout", "--quiet", "--force", "HEAD"])?;
    if dirty > 0 {
        log(format!("reset {} to HEAD ({dirty} locally modified path(s) discarded)", repo.display()));
    }
    for (path, _) in gitlinks(repo)? {
        let sub = repo.join(&path);
        if sub.join(".git").is_dir() {
            reset_trees(&sub)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- files

/// Removes `path` (a tree, a file or a symlink); nothing to do when it does not exist.
fn remove_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("inspecting {}", path.display())),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display())),
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
    }
}

/// Copies the tree `from` to `to` (`cp -a` minus timestamps and ownership): directories,
/// files with their permissions, symlinks as symlinks.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)
                .with_context(|| format!("linking {}", dst.display()))?;
        } else if kind.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    let perms = std::fs::metadata(from)?.permissions();
    std::fs::set_permissions(to, perms)?;
    Ok(())
}

/// Every regular file under `dir` (`find -type f`: symlinks are not followed or listed).
fn list_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            list_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

/// The lowercase hex SHA-256 of a file.
fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Sha256::digest(&bytes).iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// `file -b <path>`, or why it could not be run.
fn file_type(path: &Path) -> String {
    match Command::new("file").arg("-b").arg(path).stderr(Stdio::null()).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        Ok(o) => format!("(file(1) failed: {})", o.status),
        Err(e) => format!("(file(1) not available: {e})"),
    }
}

/// The `commit = "<sha>"` value(s) of `UPSTREAM.lock` (`awk -F'"' '/^commit/ { print $2 }'`).
pub fn lock_commit(lock: &str) -> String {
    lock.lines()
        .filter(|l| l.starts_with("commit"))
        .filter_map(|l| l.split('"').nth(1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Now as `YYYY-MM-DDTHH:MM:SSZ` (`date -u +%Y-%m-%dT%H:%M:%SZ`).
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    utc_timestamp(secs)
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ` (the proleptic Gregorian civil date,
/// Howard Hinnant's `civil_from_days`).
pub fn utc_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(env: &[(&str, &str)]) -> Config {
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::new(
            |k| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()),
            Path::new("/wt"),
            Path::new("/main"),
            || PathBuf::from("/project"),
        )
    }

    fn strs(v: &[OsString]) -> Vec<&str> {
        v.iter().map(|s| s.to_str().unwrap()).collect()
    }

    #[test]
    fn defaults_follow_the_checkout_and_the_main_checkout() {
        let c = cfg(&[]);
        assert_eq!(c.deps, Path::new("/wt/target/metamod-deps"));
        assert_eq!(c.out, Path::new("/wt/target/metamod"));
        assert_eq!(c.reference, Path::new("/main/target/ext/metamod-source"));
        assert_eq!(c.xwin, Path::new("/main/target/xwin"));
        assert_eq!(c.mm_commit, MM_COMMIT);
        assert_eq!(c.kh_commit, KH_COMMIT);
        assert_eq!((c.image.as_str(), c.base_image.as_str()), (IMAGE, BASE_IMAGE));
        assert!(!c.force_image);
        assert_eq!(c.network, "host");
        assert_eq!(c.podman, "podman");
        assert_eq!(c.xdg_runtime_dir, Path::new("/project/.podman/xdg"));
        assert_eq!(c.containerfile(), Path::new("/wt/crates/metamod/Containerfile"));
    }

    #[test]
    fn overrides_win_and_empty_values_are_unset() {
        let c = cfg(&[
            ("MM_COMMIT", "abc"),
            ("MM_REF", "/ref"),
            ("XWIN", "/x"),
            ("MM_IMAGE", "img"),
            ("MM_BASE_IMAGE", "base"),
            ("MM_FORCE_IMAGE", "1"),
            ("MM_NETWORK", ""),
            ("XDG_RUNTIME_DIR", "/run/user/1"),
            ("PODMAN", "/stub/podman"),
        ]);
        assert_eq!(c.mm_commit, "abc");
        assert_eq!(c.reference, Path::new("/ref"));
        assert_eq!(c.xwin, Path::new("/x"));
        assert_eq!((c.image.as_str(), c.base_image.as_str()), ("img", "base"));
        assert!(c.force_image);
        assert_eq!(c.network, "host");
        assert_eq!(c.xdg_runtime_dir, Path::new("/run/user/1"));
        assert_eq!(c.podman, "/stub/podman");
        assert!(!cfg(&[("MM_FORCE_IMAGE", "yes")]).force_image);
    }

    #[test]
    fn the_container_command_lines() {
        let c = cfg(&[("MM_NETWORK", "pasta")]);
        assert_eq!(
            strs(&c.build_run_argv("linux", 42)),
            [
                "run", "--rm", "-i", "--name", "mmsrc-linux-42", "--label", "cs2mcp.metamod=linux", "--network", "pasta",
                "-v", "/wt/target/metamod-deps:/deps", IMAGE, "bash", "-s", "--", "linux",
            ]
        );
        assert_eq!(
            strs(&c.build_run_argv("win64", 42)),
            [
                "run", "--rm", "-i", "--name", "mmsrc-win64-42", "--label", "cs2mcp.metamod=win64", "--network", "pasta",
                "-v", "/wt/target/metamod-deps:/deps", "-v", "/main/target/xwin:/xwin:ro", IMAGE, "bash", "-s", "--", "win64",
            ]
        );
        assert_eq!(
            strs(&c.probe_run_argv(7)),
            [
                "run", "--rm", "-i", "--name", "mmsrc-probe-7", "--label", "cs2mcp.metamod=probe", "--network", "pasta",
                "-v", "/main/target/xwin:/xwin:ro", IMAGE, "bash", "-s",
            ]
        );
        assert_eq!(
            strs(&c.image_build_argv()),
            [
                "build", "-t", IMAGE, "--build-arg", &format!("BASE_IMAGE={BASE_IMAGE}"), "-f",
                "/wt/crates/metamod/Containerfile", "/wt/target/metamod-deps/image-context",
            ]
        );
    }

    #[test]
    fn the_embedded_container_scripts_are_the_bash_halves() {
        for script in [CONTAINER_BUILD, CONTAINER_PROBE] {
            assert!(script.contains("\nset -euo pipefail\n"), "strict mode");
        }
        assert!(CONTAINER_BUILD.contains("platform=$1"));
        assert!(CONTAINER_BUILD.contains("cp -a \"$build/package/addons/.\" /deps/stage-$platform/addons/"));
        assert!(CONTAINER_BUILD.contains("> /deps/buildinfo-$platform.txt"));
        assert!(CONTAINER_PROBE.contains("lld-link /DLL /OUT:probe.dll"));
    }

    #[test]
    fn gitlinks_are_the_160000_records() {
        let ls = "100644 blob 1111\tREADME.md\x00160000 commit 2222\tthird_party/khook\x00\
                  040000 tree 3333\tsupport\x00160000 commit 4444\tpath with space\x00";
        assert_eq!(
            parse_gitlinks(ls),
            [("third_party/khook".into(), "2222".into()), ("path with space".into(), "4444".into())]
        );
    }

    #[test]
    fn the_lock_commit_is_the_quoted_value() {
        let lock = "# c\nrepo = \"https://x\"\ncommit = \"d88f0f\"\ndate = \"2026-11-03\"\n";
        assert_eq!(lock_commit(lock), "d88f0f");
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_timestamp(1_793_900_000), "2026-11-05T17:33:20Z");
    }

    #[test]
    fn the_stop_text_names_the_new_commands() {
        assert!(WIN64_BLOCKED.contains("cargo bridge metamod win64-probe"));
        assert!(!WIN64_BLOCKED.contains("build-metamod.sh"));
    }
}
