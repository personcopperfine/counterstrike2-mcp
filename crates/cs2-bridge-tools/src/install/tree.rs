//! The Linux tree's file operations, run inside `podman unshare` (the `cs2-home` volume's files
//! belong to a subuid). The outer command spawns `podman unshare <this binary> install
//! --platform linux tree-op --request <json>`; this side does exactly one [`Req`] with plain
//! file calls, prints its progress lines on stderr and ONE JSON result line on stdout. Every
//! path travels in the JSON request, never into a script body. The lock stays with the outer
//! process (its descriptor is close-on-exec, so this side never holds it).
//!
//! The decisions stay outside: which artefacts exist on the host, what the records say, whether
//! the guard allows the write. This side only reads and writes the tree (and, for the backup,
//! copies tree files out, so they come out owned by the host user).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::fsx::{self, copy_p, die, is_our_temp_name, say, sha_file, OrDie, Stop, R};
use super::records::{NoSource, Records, Wrote};
use super::text::LINUX_VDF;
use super::Ctx;

pub const BRIDGE_SO: &str = "csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so";
pub const MMLOAD_SO: &str = "csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so";
pub const VDF: &str = "csgo/addons/metamod/cs2mcp.vdf";
const METAMOD_DIR: &str = "csgo/addons/metamod";
const CS2MCP_DIR: &str = "csgo/addons/cs2mcp";

/// One operation on the Linux game dir `game`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Req {
    /// Does the game dir / `csgo/` exist, and which of `files` are regular files?
    Probe { game: PathBuf, files: Vec<String> },
    /// --dry-run: list the files a backup would copy.
    BackupList { game: PathBuf },
    /// Copy every pre-existing file of ours out into `partial` and write its SHA256SUMS, an empty
    /// ANCHORS and the PLANNED INSTALLED / WROTE_OVER lists. `lib` / `mmload`: the host builds
    /// that will be copied (`None`: not built, the installed copy stays).
    Backup {
        game: PathBuf,
        partial: PathBuf,
        mm: PathBuf,
        lib: Option<PathBuf>,
        mmload: Option<PathBuf>,
    },
    /// Lay the Metamod build, the two libraries and the vdf down; returns what was created and
    /// overwritten.
    Install {
        game: PathBuf,
        mm: PathBuf,
        lib: PathBuf,
        mmload: PathBuf,
        fault: Option<String>,
    },
    /// Uninstall's tree work: restore `paths` through the chain over `wro`, delete `rels` (the
    /// INSTALLED union minus the pre-install files), rmdir the empty `dirs`.
    Unwind {
        game: PathBuf,
        fault: Option<String>,
        wro: Vec<Wrote>,
        paths: Vec<String>,
        rels: Vec<String>,
        dirs: Vec<String>,
    },
    /// status lines; the libraries say which candidate build they match.
    Status {
        game: PathBuf,
        lib_cands: Vec<PathBuf>,
        mmload_cands: Vec<PathBuf>,
    },
    /// ensure's read-only check; returns `{issues, fatal}`.
    Ensure {
        game: PathBuf,
        mm: PathBuf,
        lib: Option<PathBuf>,
        mmload: Option<PathBuf>,
    },
}

/// Run `req` inside `podman unshare`. `Err(code)`: the op exited non-zero (99: a fault point
/// fired, which the caller propagates as such).
pub fn lrun(ctx: &Ctx, req: &Req) -> Result<Value, u8> {
    let Ok(exe) = std::env::current_exe() else {
        say("FAIL: cannot locate this binary to run the tree op");
        return Err(1);
    };
    let request = serde_json::to_string(req).map_err(|_| 1u8)?;
    let out = Command::new(&ctx.podman)
        .arg("unshare")
        .arg(exe)
        .args([
            "install",
            "--platform",
            "linux",
            "tree-op",
            "--request",
            &request,
        ])
        .env("XDG_RUNTIME_DIR", &ctx.xdg)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output();
    let out = match out {
        Ok(out) => out,
        Err(e) => {
            say(format_args!(
                "FAIL: cannot run {} unshare: {e}",
                ctx.podman.to_string_lossy()
            ));
            return Err(1);
        }
    };
    if !out.status.success() {
        return Err(out
            .status
            .code()
            .map(|c| c.clamp(1, 255) as u8)
            .unwrap_or(1));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| l.starts_with('{'))
        .unwrap_or("{}");
    serde_json::from_str(line).map_err(|_| {
        say("FAIL: the tree op returned no result");
        1
    })
}

/// The `tree-op` command: one request, one JSON result line.
pub fn serve(request: Option<&str>) -> R<u8> {
    let Some(request) = request else {
        say("FAIL: tree-op needs --request");
        return Err(Stop(2));
    };
    let req: Req = serde_json::from_str(request).map_err(|e| {
        say(format_args!("FAIL: bad tree-op request: {e}"));
        Stop(2)
    })?;
    let result = match req {
        Req::Probe { game, files } => probe(&game, &files),
        Req::BackupList { game } => {
            for rel in ours_present(&game) {
                say(format_args!("would back up: {rel}"));
            }
            json!({})
        }
        Req::Backup {
            game,
            partial,
            mm,
            lib,
            mmload,
        } => backup(&game, &partial, &mm, lib.as_deref(), mmload.as_deref())?,
        Req::Install {
            game,
            mm,
            lib,
            mmload,
            fault,
        } => install(&game, &mm, &lib, &mmload, fault.as_deref())?,
        Req::Unwind {
            game,
            fault,
            wro,
            paths,
            rels,
            dirs,
        } => unwind(&game, fault.as_deref(), wro, &paths, &rels, &dirs)?,
        Req::Status {
            game,
            lib_cands,
            mmload_cands,
        } => status(&game, &lib_cands, &mmload_cands),
        Req::Ensure {
            game,
            mm,
            lib,
            mmload,
        } => ensure(&game, &mm, lib.as_deref(), mmload.as_deref()),
    };
    println!("{result}");
    Ok(0)
}

fn rel_of(game: &Path, p: &Path) -> String {
    p.strip_prefix(game)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

fn probe(game: &Path, files: &[String]) -> Value {
    json!({
        "game_dir": game.is_dir(),
        "csgo_dir": game.join("csgo").is_dir(),
        "files": files.iter().map(|f| game.join(f).is_file()).collect::<Vec<bool>>(),
    })
}

/// The regular files under our two addons dirs, minus a killed run's temps (never backed up,
/// never restored).
fn ours_present(game: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for dir in [METAMOD_DIR, CS2MCP_DIR] {
        for f in fsx::walk_files(&game.join(dir)) {
            let name = f
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_our_temp_name(&name) {
                continue;
            }
            let rel = rel_of(game, &f);
            if !out.contains(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

/// `(game-relative dest, build file)` for every file of the Metamod build dir.
fn mm_files(mm: &Path) -> Vec<(String, PathBuf)> {
    let src = mm.join("metamod");
    fsx::walk_files(&src)
        .into_iter()
        .map(|f| (format!("{METAMOD_DIR}/{}", rel_of(&src, &f)), f))
        .collect()
}

fn is_metaplugins(rel: &str) -> bool {
    Path::new(rel)
        .file_name()
        .is_some_and(|n| n == "metaplugins.ini")
}

fn backup(
    game: &Path,
    partial: &Path,
    mm: &Path,
    lib: Option<&Path>,
    mmload: Option<&Path>,
) -> R<Value> {
    let w = |name: &str, body: &str| {
        fs::write(partial.join(name), body)
            .or_die(|| format!("cannot write {name} into {}", partial.display()))
    };
    let mut sums = String::new();
    for rel in ours_present(game) {
        let dst = partial.join(&rel);
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d).or_die(|| format!("cannot create {}", d.display()))?;
        }
        copy_p(&game.join(&rel), &dst).or_die(|| format!("cannot back up {rel}"))?;
        say(format_args!("backed up: {rel}"));
        let sha = sha_file(&dst).or_die(|| format!("cannot hash the backup of {rel}"))?;
        sums.push_str(&format!("{sha}  {rel}\n"));
    }
    w("SHA256SUMS", &sums)?;
    // a linux install relies on no Valve-owned line (no game file is edited): the file stays,
    // empty, so every backup has the same shape
    w("ANCHORS", "")?;
    let sha = |p: &Path| sha_file(p).or_die(|| format!("cannot hash {}", p.display()));
    let mm = mm_files(mm);
    let mut installed = String::new();
    for rel in [VDF, BRIDGE_SO, MMLOAD_SO] {
        if !game.join(rel).exists() {
            installed.push_str(&format!("{rel}\n"));
        }
    }
    for (rel, src) in &mm {
        if game.join(rel).exists() {
            continue;
        }
        if is_metaplugins(rel) {
            installed.push_str(&format!("{rel} {}\n", sha(src)?));
        } else {
            installed.push_str(&format!("{rel}\n"));
        }
    }
    w("INSTALLED", &installed)?;
    let mut wro = String::new();
    if game.join(METAMOD_DIR).is_dir() {
        for (rel, src) in &mm {
            if !is_metaplugins(rel) && game.join(rel).exists() {
                wro.push_str(&format!("{rel} {}\n", sha(src)?));
            }
        }
    }
    if game.join(VDF).exists() {
        wro.push_str(&format!("{VDF} {}\n", fsx::sha_bytes(LINUX_VDF.as_bytes())));
    }
    for (rel, build) in [(BRIDGE_SO, lib), (MMLOAD_SO, mmload)] {
        if let Some(build) = build {
            if game.join(rel).exists() {
                wro.push_str(&format!("{rel} {}\n", sha(build)?));
            }
        }
    }
    w("WROTE_OVER", &wro)?;
    Ok(json!({}))
}

/// What `put` wrote: the INSTALLED (`Created`) or WROTE_OVER (`Wrote`) line.
enum Put {
    Created(String),
    Wrote(String),
}

/// What `put` writes: a build file, or bytes we produce (the vdf).
enum Src<'a> {
    File(&'a Path),
    Bytes(&'a [u8]),
}

/// Atomic write to `rel`: a temp in the dest's dir, owner/mode kept via a `cp -p` of the
/// original when it exists (a created file gets the build file's mode), then the fault-hooked
/// rename.
fn put(game: &Path, src: Src, rel: &str, fault: Option<&str>) -> R<Put> {
    let dest = game.join(rel);
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d).or_die(|| format!("cannot create {}", d.display()))?;
    }
    fsx::sweep_one(&dest, rel);
    let existed = dest.exists();
    let t = fsx::tmp_for(&dest).or_die(|| format!("cannot create a temp for {rel}"))?;
    match src {
        Src::File(src) if !existed => {
            copy_p(src, &t).or_die(|| format!("cannot copy {}", src.display()))?
        }
        Src::File(src) => {
            let bytes = fs::read(src).or_die(|| format!("cannot read {}", src.display()))?;
            fsx::write_into(&t, &bytes).or_die(|| format!("cannot write {}", t.display()))?;
        }
        Src::Bytes(bytes) => {
            fsx::write_into(&t, bytes).or_die(|| format!("cannot write {}", t.display()))?
        }
    }
    // the sha of the bytes WE wrote (the temp, before the rename)
    let sha = sha_file(&t).or_die(|| format!("cannot hash {}", t.display()))?;
    fsx::commit_tmp(fault, &t, &dest, rel)?;
    Ok(if existed {
        Put::Wrote(format!("{rel} {sha}"))
    } else if is_metaplugins(rel) {
        Put::Created(format!("{rel} {sha}"))
    } else {
        Put::Created(rel.to_string())
    })
}

fn install(game: &Path, mm: &Path, lib: &Path, mmload: &Path, fault: Option<&str>) -> R<Value> {
    let addons = game.join("csgo/addons");
    fs::create_dir_all(&addons).or_die(|| format!("cannot create {}", addons.display()))?;
    if game.join(METAMOD_DIR).is_dir() {
        say(format_args!(
            "existing: {METAMOD_DIR}/ (only our files overwrite ours)"
        ));
    } else {
        say(format_args!(
            "created: {METAMOD_DIR}/ (from the metamod build)"
        ));
    }
    let mut created: Vec<String> = Vec::new();
    let mut wrote: Vec<String> = Vec::new();
    let mut record = |p: Put| match p {
        Put::Created(l) => created.push(l),
        Put::Wrote(l) => wrote.push(l),
    };
    for (rel, src) in mm_files(mm) {
        // Metamod's config, not ours: created only when absent, never overwritten
        if is_metaplugins(&rel) && game.join(METAMOD_DIR).join("metaplugins.ini").is_file() {
            say(format_args!("kept: {METAMOD_DIR}/metaplugins.ini (NOT ours - Metamod's config; never overwritten)"));
            continue;
        }
        record(put(game, Src::File(&src), &rel, fault)?);
    }
    for (src, rel) in [(lib, BRIDGE_SO), (mmload, MMLOAD_SO)] {
        if src.is_file() {
            record(put(game, Src::File(src), rel, fault)?);
            say(format_args!("installed: {rel}"));
        } else if game.join(rel).is_file() {
            say(format_args!(
                "kept: {rel} (no built copy; the installed one stays)"
            ));
        } else {
            return Err(die(format_args!(
                "no built {} at the host and none installed - build it first (`cargo bridge build`)",
                rel.rsplit('/').next().unwrap_or(rel)
            )));
        }
    }
    // the vdf: exactly the bytes ensure compares against (one producer)
    record(put(game, Src::Bytes(LINUX_VDF.as_bytes()), VDF, fault)?);
    say(format_args!("created: {VDF}"));
    Ok(json!({"created": created, "wrote_over": wrote}))
}

fn unwind(
    game: &Path,
    fault: Option<&str>,
    wro: Vec<Wrote>,
    paths: &[String],
    rels: &[String],
    dirs: &[String],
) -> R<Value> {
    for rel in paths.iter().chain(rels.iter()) {
        let rel = rel.split(' ').next().unwrap_or(rel);
        fsx::sweep_one(&game.join(rel), rel);
    }
    let rec = Records {
        wro,
        ..Records::default()
    };
    // 1) restore what install overwrote (the chain rule); gone/changed paths are left alone
    for rel in paths {
        let dest = game.join(rel);
        if !dest.parent().is_some_and(Path::is_dir) {
            say(format_args!("not restored: {rel} (its directory is gone - removed by its owner after install; not recreated)"));
            continue;
        }
        if !dest.is_file() {
            say(format_args!(
                "not restored: {rel} (gone after install - removed by its owner?; not recreated)"
            ));
            continue;
        }
        let cur = sha_file(&dest).or_die(|| format!("cannot hash {rel}"))?;
        let src = match rec.chain_src_for(rel, &cur) {
            Ok(src) => src,
            Err(NoSource::Changed) => {
                say(format_args!(
                    "not restored: {rel} (changed after install - Steam/user?; left as it is now)"
                ));
                continue;
            }
            Err(NoSource::Missing) => {
                say(format_args!(
                    "WARN: not restored: {rel} (the backup copy is missing)"
                ));
                continue;
            }
        };
        let mut rt = dest.as_os_str().to_owned();
        rt.push(format!(".tmp.{}", std::process::id()));
        let rt = PathBuf::from(rt);
        fsx::register_temp(&rt);
        copy_p(&src, &rt).or_die(|| format!("cannot copy the backup of {rel}"))?;
        fsx::commit_tmp(fault, &rt, &dest, &format!("uninstall-restore:{rel}"))?;
        if !fsx::same_bytes(&src, &dest) {
            say(format_args!(
                "WARN: restore of {rel} differs from the backup copy"
            ));
        }
        say(format_args!("restored (install overwrote it): {rel}"));
    }
    // 2) delete exactly what install created; a created metaplugins.ini only while unchanged
    for entry in rels {
        let (rel, psha) = match entry.split_once(' ') {
            Some((r, s)) => (r, Some(s)),
            None => (entry.as_str(), None),
        };
        let dest = game.join(rel);
        let changed = psha.is_some() && dest.exists() && sha_file(&dest).ok().as_deref() != psha;
        if is_metaplugins(rel) && changed {
            say(format_args!(
                "kept: {rel} (NOT ours - deleted only when we created it and it is unchanged)"
            ));
        } else if dest.exists() {
            fs::remove_file(&dest).or_die(|| format!("cannot remove {rel}"))?;
            say(format_args!("removed (added by install): {rel}"));
        } else {
            say(format_args!("was not installed: {rel}"));
        }
    }
    for d in dirs {
        let p = game.join(d);
        if p.is_dir() && fs::remove_dir(&p).is_ok() {
            say(format_args!("removed empty dir: {d}"));
        }
    }
    Ok(json!({}))
}

fn status(game: &Path, lib_cands: &[PathBuf], mmload_cands: &[PathBuf]) -> Value {
    if !game.is_dir() {
        say(format_args!("(whole game dir {} absent)", game.display()));
        return json!({});
    }
    let mm = game.join(METAMOD_DIR);
    if mm.is_dir() {
        say(format_args!(
            "metamod dir {METAMOD_DIR}  present ({} files)",
            fsx::walk_files(&mm).len()
        ));
    } else {
        say(format_args!("metamod dir {METAMOD_DIR}  absent"));
    }
    for (rel, cands) in [
        (BRIDGE_SO, Some(lib_cands)),
        (MMLOAD_SO, Some(mmload_cands)),
        (VDF, None),
    ] {
        let p = game.join(rel);
        if !p.is_file() {
            say(format_args!("{rel}  absent"));
            continue;
        }
        let sha = sha_file(&p).unwrap_or_default();
        match cands {
            Some(c) => say(format_args!(
                "{rel}  present  sha256={sha}  {}",
                super::lib_build_match(&sha, c)
            )),
            None => say(format_args!("{rel}  present  sha256={sha}")),
        }
    }
    json!({})
}

/// One ensure line: `  <state>  <what>[  [why]]`.
fn report(state: &str, what: &str, why: &str) {
    if why.is_empty() {
        super::ensure_report(state, what);
    } else {
        super::ensure_report(state, format_args!("{what}  [{why}]"));
    }
}

fn ensure(game: &Path, mm: &Path, lib: Option<&Path>, mmload: Option<&Path>) -> Value {
    if !game.is_dir() {
        report(
            "missing",
            &format!("(whole game dir {} absent - install needs it; if a .so below is missing too, repair is fatal)", game.display()),
            "",
        );
        return json!({"issues": true, "fatal": false});
    }
    let (mut issues, mut fatal) = (false, false);
    for (rel, build, what) in [(BRIDGE_SO, lib, ".so"), (MMLOAD_SO, mmload, "mmload .so")] {
        let f = game.join(rel);
        let label = format!("{what} {rel}");
        if f.is_file() {
            match build {
                None => report(
                    "ok",
                    &label,
                    "no built copy to compare: current by definition",
                ),
                Some(b) if fsx::same_bytes(&f, b) => report("ok", &label, ""),
                Some(b) => {
                    report(
                        "stale:",
                        &label,
                        &format!("differs from the built {}", b.display()),
                    );
                    issues = true;
                }
            }
        } else {
            let why = if build.is_some() {
                ""
            } else {
                "no built copy either - BUILD IT FIRST (`cargo bridge build`)"
            };
            report("missing", &label, why);
            issues = true;
            fatal |= build.is_none();
        }
    }
    let src_root = mm.join("metamod");
    for (rel, src) in mm_files(mm) {
        let g = game.join(&rel);
        let what = format!("metamod {rel}");
        // metaplugins.ini is Metamod's config, not ours: presence only
        if is_metaplugins(&rel) && src.parent() == Some(src_root.as_path()) {
            if g.is_file() {
                report("ok", &what, "present; NOT ours - content never compared");
            } else {
                report("missing", &what, "");
                issues = true;
            }
            continue;
        }
        if !g.is_file() {
            report("missing", &what, "");
            issues = true;
        } else if fsx::same_bytes(&src, &g) {
            report("ok", &what, "");
        } else {
            report("stale:", &what, "differs from the metamod build");
            issues = true;
        }
    }
    match fs::read(game.join(VDF)) {
        Ok(b) if b == LINUX_VDF.as_bytes() => report("ok", VDF, ""),
        Ok(_) => {
            report("stale:", VDF, "differs from the expected vdf");
            issues = true;
        }
        Err(_) => {
            report("missing", VDF, "");
            issues = true;
        }
    }
    json!({"issues": issues, "fatal": fatal})
}
