//! `--platform wine`: the shared Wine tools install (host files, no namespace).

use std::fs;
use std::path::Path;

use super::fsx::{self, commit_tmp, copy_p, die, say, sha_file, tmp_for, write_into, OrDie, R};
use super::records::{self, backups_for_app, verify_backup_sha, NoSource, Records};
use super::text::{self, Removal, WINE_VDF};
use super::{ensure_repair, ensure_report, guard, lib_build_match, retire_backups, Ctx};

// Everything install writes, relative to the app dir. The backup, uninstall and status lists
// come from these, so they stay in sync with the install steps.
const TOOL_DLL: &str = "game/bin/win64/tools/cs2_bridge.dll";
const MANIFEST: &str = "game/bin/sdkenginetools.txt";
const MM_LOADER: &str = "game/csgo/addons/metamod/bin/win64/server.dll";
const MM_CORE: &str = "game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll";
const METAPLUGINS: &str = "game/csgo/addons/metamod/metaplugins.ini";
const VDF: &str = "game/csgo/addons/metamod/cs2mcp.vdf";
const LAUNCH_EXE: &str = "game/bin/win64/cs2mcp_launch.exe";
const LOAD_DLL: &str = "game/bin/win64/cs2mcp_mmload.dll";
const MM_DESTS: [&str; 4] = [MM_LOADER, MM_CORE, METAPLUGINS, VDF];
const LAUNCH_DESTS: [&str; 2] = [LAUNCH_EXE, LOAD_DLL];
/// Every dest, in the backup's order.
const ALL_DESTS: [&str; 8] = [
    TOOL_DLL,
    MANIFEST,
    MM_LOADER,
    MM_CORE,
    METAPLUGINS,
    VDF,
    LAUNCH_EXE,
    LOAD_DLL,
];
/// Installs made before the single-image vdf also copied the DLL where the old vdf pointed. No
/// install creates it any more; uninstall still removes it from a tree an old install left.
const LEGACY_COPY: &str = "game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll";
const LEGACY_DIRS: [&str; 3] = [
    "game/csgo/addons/cs2mcp/bin/win64",
    "game/csgo/addons/cs2mcp/bin",
    "game/csgo/addons/cs2mcp",
];
/// Bottom-up, so `rmdir` (empty only) unwinds exactly what install created. `game/bin/win64`
/// and `tools` pre-exist in a real tree (other tools' DLLs live there) and simply stay.
const MM_DIRS: [&str; 6] = [
    "game/csgo/addons/metamod/bin/win64",
    "game/csgo/addons/metamod/bin",
    "game/csgo/addons/metamod",
    "game/csgo/addons",
    "game/bin/win64/tools",
    "game/bin/win64",
];

fn key(ctx: &Ctx) -> String {
    ctx.app.to_string_lossy().into_owned()
}

fn bks(ctx: &Ctx) -> Vec<std::path::PathBuf> {
    backups_for_app(&ctx.backup_root, ctx.bk_prefix, &key(ctx))
}

fn read_text(path: &Path) -> R<String> {
    fs::read_to_string(path).or_die(|| format!("cannot read {}", path.display()))
}

/// What one install created and overwrote (the actual INSTALLED / WROTE_OVER lists).
#[derive(Default)]
struct Written {
    created: Vec<String>,
    wrote_over: Vec<String>,
}

// ----- preflight (read-only; every refusal before anything is touched) ----------------------------

fn preflight_artefact(ctx: &Ctx, build: &Path, rel: &str, what: &str) -> R<()> {
    if build.is_file() {
        return Ok(());
    }
    if ctx.app.join(rel).is_file() {
        say(format_args!(
            "no built {what} at {} - the INSTALLED copy counts as current (only config is repaired)",
            build.display()
        ));
        return Ok(());
    }
    Err(die(format_args!(
        "no {what} at {} and none installed at {} - BUILD IT FIRST (run `cargo bridge build`)",
        build.display(),
        ctx.app.join(rel).display()
    )))
}

fn preflight(ctx: &Ctx) -> R<()> {
    ctx.say_build_checkout(&ctx.rs_lib);
    if ctx.rs_lib.is_file() {
        ctx.export_gate(&ctx.rs_lib, "DLL")?;
    } else if ctx.app.join(TOOL_DLL).is_file() {
        say(format_args!(
            "no built DLL at {} - the INSTALLED tool DLL counts as current (only config is repaired)",
            ctx.rs_lib.display()
        ));
    } else {
        return Err(die(format_args!(
            "no Rust DLL at {} and none installed at {} - BUILD IT FIRST (`cargo bridge build`, or point CS2_BRIDGE_DLL at one)",
            ctx.rs_lib.display(),
            ctx.app.join(TOOL_DLL).display()
        )));
    }
    preflight_artefact(ctx, &ctx.launch_exe, LAUNCH_EXE, "launcher exe")?;
    preflight_artefact(ctx, &ctx.load_dll, LOAD_DLL, "mmload DLL")?;
    if !ctx.man.is_file() {
        return Err(die(format_args!(
            "missing {} (game tree incomplete: install only EDITS existing files)",
            ctx.app.join(MANIFEST).display()
        )));
    }
    if text::apply_manifest_edit(&read_text(&ctx.man)?).is_none() {
        return Err(die(format_args!(
            "{}: no 'hammer' entry found to insert the cs2_bridge entry before (unexpected manifest layout)",
            ctx.man.display()
        )));
    }
    for f in [
        "metamod/bin/win64/server.dll",
        "metamod/bin/win64/metamod.2.cs2.dll",
        "metamod/metaplugins.ini",
    ] {
        let p = ctx.mm_build.join(f);
        if !p.is_file() {
            return Err(die(format_args!(
                "missing metamod build file: {} (crates/metamod/fetch-metamod.sh win64 fetches it, or point CS2_MM at a build)",
                p.display()
            )));
        }
    }
    Ok(())
}

// ----- backup ----------------------------------------------------------------------------------------

/// Copies of every dest that already exists, plus the planned lists (see [`records`]), built in
/// `<bk>.partial` and renamed into place: a killed install never leaves a half backup, and a
/// kill after the rename still leaves a tree uninstall can take apart (the PLANNED INSTALLED
/// and WROTE_OVER lists; the actual ones are written at the end of install).
fn backup(ctx: &Ctx, bk: &Path) -> R<()> {
    let mut partial = bk.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);
    fsx::set_partial(Some(partial.clone()));
    let w = |name: &str, body: &str| {
        fs::write(partial.join(name), body)
            .or_die(|| format!("cannot write {name} into {}", partial.display()))
    };
    fs::create_dir_all(&partial).or_die(|| format!("cannot create {}", partial.display()))?;
    w("APP_TREE", &format!("{}\n", key(ctx)))?;
    let mut sums = String::new();
    for rel in ALL_DESTS {
        let src = ctx.app.join(rel);
        if !src.exists() {
            continue;
        }
        let dst = partial.join(rel);
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d).or_die(|| format!("cannot create {}", d.display()))?;
        }
        copy_p(&src, &dst).or_die(|| format!("cannot back up {rel}"))?;
        let sha = sha_file(&dst).or_die(|| format!("cannot hash the backup of {rel}"))?;
        sums.push_str(&format!("{sha}  {rel}\n"));
        say(format_args!("  backed up: {rel}"));
    }
    if sums.is_empty() {
        say("  (nothing pre-existed; the backup still records EDITS_WROTE/INSTALLED)");
    }
    w("SHA256SUMS", &sums)?;
    let manifest = fs::read(&ctx.man).ok();
    let mut edits = String::new();
    if let Some(m) = &manifest {
        if !String::from_utf8_lossy(m).contains(text::ENTRY_MARKER) {
            edits.push_str("manifest:cs2_bridge\n");
        }
    }
    w("EDITS_WROTE", &edits)?;
    // the Valve-owned line install relies on, as it reads now: ensure re-checks it, so a file
    // truncated after our entry reads "game file damaged", not current
    let anchors = manifest
        .as_deref()
        .and_then(|m| text::manifest_anchor_line(&String::from_utf8_lossy(m)))
        .map(|a| format!("manifest\t{a}\n"))
        .unwrap_or_default();
    w("ANCHORS", &anchors)?;
    let mm_sha = |f: &str| {
        let p = ctx.mm_build.join(f);
        sha_file(&p).or_die(|| format!("cannot hash {}", p.display()))
    };
    let mut installed = String::new();
    for rel in [TOOL_DLL]
        .iter()
        .chain(MM_DESTS.iter())
        .chain(LAUNCH_DESTS.iter())
    {
        if ctx.app.join(rel).exists() {
            continue;
        }
        if *rel == METAPLUGINS {
            installed.push_str(&format!("{rel} {}\n", mm_sha("metamod/metaplugins.ini")?));
        } else {
            installed.push_str(&format!("{rel}\n"));
        }
    }
    w("INSTALLED", &installed)?;
    // the pre-existing paths install would overwrite, with the sha of the bytes WE would write;
    // the manifest is not here (its edit is reverted from the current file), metaplugins.ini
    // never is (create-if-absent only)
    let mut wro = String::new();
    let mut plan = |rel: &str, sha: String| {
        if ctx.app.join(rel).exists() {
            wro.push_str(&format!("{rel} {sha}\n"));
        }
    };
    for (rel, build) in [
        (TOOL_DLL, &ctx.rs_lib),
        (LAUNCH_EXE, &ctx.launch_exe),
        (LOAD_DLL, &ctx.load_dll),
    ] {
        if build.is_file() {
            plan(
                rel,
                sha_file(build).or_die(|| format!("cannot hash {}", build.display()))?,
            );
        }
    }
    plan(MM_LOADER, mm_sha("metamod/bin/win64/server.dll")?);
    plan(MM_CORE, mm_sha("metamod/bin/win64/metamod.2.cs2.dll")?);
    plan(VDF, fsx::sha_bytes(WINE_VDF.as_bytes()));
    w("WROTE_OVER", &wro)?;
    fsx::fault(ctx.fault_cfg(), "mid-backup")?;
    fs::rename(&partial, bk).or_die(|| format!("cannot finalise the backup {}", bk.display()))?;
    fsx::set_partial(None);
    Ok(())
}

/// Rewrite one of the backup's lists atomically (temp + rename).
fn write_list(bk: &Path, name: &str, lines: &[String]) -> R<()> {
    let dest = bk.join(name);
    let t = tmp_for(&dest).or_die(|| format!("cannot write {}", dest.display()))?;
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    write_into(&t, body.as_bytes()).or_die(|| format!("cannot write {}", t.display()))?;
    fs::rename(&t, &dest).or_die(|| format!("cannot write {}", dest.display()))
}

// ----- install steps ---------------------------------------------------------------------------------

/// Atomic copy of a build artefact to `rel`.
fn copy_in(ctx: &Ctx, src: &Path, rel: &str, out: &mut Written) -> R<()> {
    if ctx.dry {
        say(format_args!("DRY: cp {} -> {rel}", src.display()));
        return Ok(());
    }
    let dest = ctx.app.join(rel);
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d).or_die(|| format!("cannot create {}", d.display()))?;
    }
    let existed = dest.exists();
    let t = tmp_for(&dest).or_die(|| format!("cannot create a temp for {rel}"))?;
    if existed {
        let bytes = fs::read(src).or_die(|| format!("cannot read {}", src.display()))?;
        write_into(&t, &bytes).or_die(|| format!("cannot write {}", t.display()))?;
    } else {
        copy_p(src, &t).or_die(|| format!("cannot copy {}", src.display()))?;
    }
    commit_tmp(ctx.fault_cfg(), &t, &dest, rel)?;
    if existed {
        let sha = sha_file(src).or_die(|| format!("cannot hash {}", src.display()))?;
        out.wrote_over.push(format!("{rel} {sha}"));
    } else {
        out.created.push(rel.into());
    }
    say(format_args!("  installed: {rel}"));
    Ok(())
}

/// Insert our entry before the hammer entry (exact text, idempotent).
fn edit_manifest(ctx: &Ctx) -> R<()> {
    if ctx.dry {
        say(format_args!(
            "DRY: {MANIFEST} - insert m_Name = \"cs2_bridge\" entry before the hammer entry"
        ));
        return Ok(());
    }
    let t = tmp_for(&ctx.man).or_die(|| format!("cannot create a temp for {MANIFEST}"))?;
    let Some(edited) = text::apply_manifest_edit(&read_text(&ctx.man)?) else {
        let _ = fs::remove_file(&t);
        return Err(die(format_args!(
            "{}: cannot insert the cs2_bridge entry (no 'hammer' anchor?)",
            ctx.man.display()
        )));
    };
    write_into(&t, edited.as_bytes()).or_die(|| format!("cannot write {}", t.display()))?;
    commit_tmp(ctx.fault_cfg(), &t, &ctx.man, MANIFEST)?;
    say(format_args!("  edited: {MANIFEST}"));
    Ok(())
}

/// The Metamod 2.0 plugin registration (format and resolution rule: [`text::WINE_VDF`]).
fn write_vdf(ctx: &Ctx, out: &mut Written) -> R<()> {
    if ctx.dry {
        say(format_args!("DRY: {VDF} - vdf \"Metamod Plugin\" {{ alias, file ../bin/win64/tools/cs2_bridge.dll }}"));
        return Ok(());
    }
    let dest = ctx.app.join(VDF);
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d).or_die(|| format!("cannot create {}", d.display()))?;
    }
    let existed = dest.exists();
    let t = tmp_for(&dest).or_die(|| format!("cannot create a temp for {VDF}"))?;
    write_into(&t, WINE_VDF.as_bytes()).or_die(|| format!("cannot write {}", t.display()))?;
    commit_tmp(ctx.fault_cfg(), &t, &dest, VDF)?;
    if existed {
        out.wrote_over
            .push(format!("{VDF} {}", fsx::sha_bytes(WINE_VDF.as_bytes())));
    } else {
        out.created.push(VDF.into());
    }
    say(format_args!("  created: {VDF}"));
    Ok(())
}

pub fn install(ctx: &Ctx) -> R<u8> {
    guard::guard_live_game(ctx)?;
    preflight(ctx)?;
    records::gameinfo_era_refusal(&Records::load(bks(ctx), false)?, &ctx.backup_root)?;
    let bk = ctx.new_backup_dir();
    if ctx.dry {
        say("dry-run: nothing below changes anything");
        say(format_args!(
            "  would back up first under: {}/{}-<UTC>",
            ctx.backup_root.display(),
            ctx.bk_prefix
        ));
        for rel in ALL_DESTS {
            if ctx.app.join(rel).exists() {
                say(format_args!(
                    "dry-run: would back up {rel} before overwriting"
                ));
            }
        }
    } else {
        let mut sweep: Vec<&str> = ALL_DESTS.to_vec();
        sweep.push(LEGACY_COPY);
        fsx::sweep_dest_temps(&ctx.app, &sweep);
        backup(ctx, &bk)?;
    }
    let mut out = Written::default();
    if ctx.rs_lib.is_file() {
        copy_in(ctx, &ctx.rs_lib, TOOL_DLL, &mut out)?;
    } else {
        say(format_args!(
            "  kept: {TOOL_DLL} (no built DLL; the installed copy stays)"
        ));
    }
    edit_manifest(ctx)?;
    for (build, rel, what) in [
        (&ctx.launch_exe, LAUNCH_EXE, "exe"),
        (&ctx.load_dll, LOAD_DLL, "DLL"),
    ] {
        if build.is_file() {
            copy_in(ctx, build, rel, &mut out)?;
        } else {
            say(format_args!(
                "  kept: {rel} (no built {what}; the installed copy stays)"
            ));
        }
    }
    copy_in(
        ctx,
        &ctx.mm_build.join("metamod/bin/win64/server.dll"),
        MM_LOADER,
        &mut out,
    )?;
    copy_in(
        ctx,
        &ctx.mm_build.join("metamod/bin/win64/metamod.2.cs2.dll"),
        MM_CORE,
        &mut out,
    )?;
    // Metamod's config, not ours (we load through cs2mcp.vdf): created only when absent
    if ctx.app.join(METAPLUGINS).is_file() {
        say(format_args!(
            "  kept: {METAPLUGINS} (NOT ours - Metamod's config; never overwritten)"
        ));
    } else {
        copy_in(
            ctx,
            &ctx.mm_build.join("metamod/metaplugins.ini"),
            METAPLUGINS,
            &mut out,
        )?;
    }
    write_vdf(ctx, &mut out)?;
    say("install done (wine: tool + launcher + metamod, one image, gameinfo untouched)");
    if !ctx.dry {
        // a created metaplugins.ini records the sha of what we created: uninstall deletes it
        // only while still byte-identical
        let created: Vec<String> = out
            .created
            .iter()
            .map(|rel| match sha_file(&ctx.app.join(rel)) {
                Ok(sha) if rel == METAPLUGINS => format!("{rel} {sha}"),
                _ => rel.clone(),
            })
            .collect();
        write_list(&bk, "INSTALLED", &created)?;
        write_list(&bk, "WROTE_OVER", &out.wrote_over)?;
        say("restart the runtime to load it: `cargo bridge runtime up` (it runs `install ensure` first and adds -insecure itself)");
    }
    Ok(0)
}

// ----- uninstall ---------------------------------------------------------------------------------------

pub fn uninstall(ctx: &Ctx) -> R<u8> {
    guard::guard_live_game(ctx)?;
    // the tree goes back to what it was before the FIRST install: the OLDEST backup
    let all = bks(ctx);
    let Some(oldest) = all.first().cloned() else {
        return Err(die(format_args!(
            "no {}-* directory under {} - refusing to uninstall without a backup",
            ctx.bk_prefix,
            ctx.backup_root.display()
        )));
    };
    verify_backup_sha(&oldest)?;
    if !oldest.join("EDITS_WROTE").is_file() {
        return Err(die(format_args!(
            "no EDITS_WROTE in {} (the install that wrote this backup did not complete) - refusing to uninstall: nothing would be removed",
            oldest.display()
        )));
    }
    // the records of EVERY backup of this tree (a later ensure repair's created files are
    // removed too), each contributing backup SHA256SUMS-verified, before anything is touched
    let rec = Records::load(all.clone(), true)?;
    if rec.inst.is_empty() {
        return Err(die(format_args!(
            "no INSTALLED list in any backup of this tree under {} (the installs that wrote them did not complete) - refusing to uninstall: nothing would be removed",
            ctx.backup_root.display()
        )));
    }
    records::gameinfo_era_refusal(&rec, &ctx.backup_root)?;
    let mut del: Vec<&str> = Vec::new();
    for line in &rec.inst {
        let rel = line.split(' ').next().unwrap_or(line);
        if rec.pre_first(rel) {
            say(format_args!(
                "  (INSTALLED: {rel} pre-existed the first install - NOT deleted; the WROTE_OVER restore puts the owner's bytes back if install overwrote it)"
            ));
        } else {
            del.push(line);
        }
    }
    if ctx.dry {
        say(format_args!(
            "dry-run: would uninstall from {} (nothing changes)",
            oldest.display()
        ));
        return Ok(0);
    }
    let mut sweep: Vec<&str> = ALL_DESTS.to_vec();
    sweep.push(LEGACY_COPY);
    fsx::sweep_dest_temps(&ctx.app, &sweep);
    say(format_args!(
        "restoring from {} (pre-first-install image)",
        oldest.display()
    ));
    // 1) restore ONLY what install overwrote, from the chain's copy; a file changed after our
    //    last write is left alone; a failed restore is a warning for that path
    for rel in rec.wrote_paths() {
        let dest = ctx.app.join(&rel);
        if !dest.is_file() {
            say(format_args!(
                "  not restored: {rel} (gone after install - removed by its owner?; not recreated)"
            ));
            continue;
        }
        let cur = sha_file(&dest).or_die(|| format!("cannot hash {rel}"))?;
        let src = match rec.chain_src_for(&rel, &cur) {
            Ok(src) => src,
            Err(NoSource::Changed) => {
                say(format_args!("  not restored: {rel} (changed after install - Steam/user?; left as it is now)"));
                continue;
            }
            Err(NoSource::Missing) => {
                say(format_args!(
                    "  WARN: not restored: {rel} (the backup copy is missing)"
                ));
                continue;
            }
        };
        let mut rt = dest.as_os_str().to_owned();
        rt.push(format!(".tmp.{}", std::process::id()));
        let rt = std::path::PathBuf::from(rt);
        fsx::register_temp(&rt);
        copy_p(&src, &rt).or_die(|| format!("cannot copy the backup of {rel}"))?;
        commit_tmp(
            ctx.fault_cfg(),
            &rt,
            &dest,
            &format!("uninstall-restore:{rel}"),
        )?;
        if fsx::same_bytes(&src, &dest) {
            say(format_args!("  restored (install overwrote it): {rel}"));
        } else {
            say(format_args!(
                "  WARN: restore of {rel} differs from the backup copy"
            ));
        }
    }
    // 2) delete exactly what install created (the INSTALLED union minus the pre-install files)
    for line in del {
        let (rel, psha) = match line.split_once(' ') {
            Some((r, s)) => (r, Some(s)),
            None => (line, None),
        };
        let dest = ctx.app.join(rel);
        if rel == METAPLUGINS {
            let ours = psha.is_some() && dest.is_file() && sha_file(&dest).ok().as_deref() == psha;
            if ours {
                fs::remove_file(&dest).or_die(|| format!("cannot remove {rel}"))?;
                say(format_args!(
                    "  removed (created by install, byte-identical to what we created): {rel}"
                ));
            } else {
                say(format_args!("  kept: {rel} (NOT ours - deleted only when we created it and it is unchanged)"));
            }
            continue;
        }
        if dest.exists() {
            fs::remove_file(&dest).or_die(|| format!("cannot remove {rel}"))?;
            say(format_args!("  removed (created by install): {rel}"));
        } else {
            say(format_args!("  (INSTALLED: {rel} already absent)"));
        }
    }
    // the second copy an old install left behind is never in INSTALLED
    let legacy = ctx.app.join(LEGACY_COPY);
    if legacy.exists() {
        fs::remove_file(&legacy).or_die(|| format!("cannot remove {LEGACY_COPY}"))?;
        say(format_args!(
            "  removed (LEGACY copy from an old install): {LEGACY_COPY}"
        ));
    }
    for d in MM_DIRS.iter().chain(LEGACY_DIRS.iter()) {
        let p = ctx.app.join(d);
        if p.is_dir() && fs::remove_dir(&p).is_ok() {
            say(format_args!("  removed empty dir: {d}"));
        }
    }
    // 3) revert our manifest edit in the CURRENT file, last (after the file work succeeded): a
    //    Steam update's other changes stay; our text no longer found -> refuse, never restore
    //    an old Valve file from the backup
    for kind in &rec.kinds {
        if kind != "manifest:cs2_bridge" {
            say(format_args!("  (unknown EDITS_WROTE line ignored: {kind})"));
            continue;
        }
        if !ctx.man.is_file() {
            return Err(die(format_args!(
                "cannot revert the manifest edit: {} is GONE (game file damaged? refusing - verify the game files; nothing restored)",
                ctx.man.display()
            )));
        }
        let t = tmp_for(&ctx.man).or_die(|| format!("cannot create a temp for {MANIFEST}"))?;
        match text::remove_manifest_entry(&read_text(&ctx.man)?) {
            Removal::Removed(new) => {
                write_into(&t, new.as_bytes())
                    .or_die(|| format!("cannot write {}", t.display()))?;
                commit_tmp(
                    ctx.fault_cfg(),
                    &t,
                    &ctx.man,
                    &format!("uninstall:{MANIFEST}"),
                )?;
                say(format_args!(
                    "  reverted: {MANIFEST} (our cs2_bridge entry removed)"
                ));
            }
            Removal::Absent => {
                let _ = fs::remove_file(&t);
                say("  (manifest: our entry already absent)");
            }
            Removal::Foreign => {
                let _ = fs::remove_file(&t);
                return Err(die(format_args!(
                    "{}: our cs2_bridge entry is there but no longer in the exact form install wrote - refusing to restore a stale Valve file (verify the game files)",
                    ctx.man.display()
                )));
            }
        }
    }
    retire_backups(ctx, &bks(ctx));
    say("uninstall complete (restore from the oldest backup + remove what install added)");
    Ok(0)
}

// ----- status ------------------------------------------------------------------------------------------

pub fn status(ctx: &Ctx) -> R<u8> {
    let real = fsx::realpath_m(&ctx.game) == fsx::realpath_m(&ctx.real_game);
    say(format_args!(
        "game: {}  [{}]",
        ctx.game.display(),
        if real {
            "REAL tools install"
        } else {
            "test tree"
        }
    ));
    let shown = |label: &str, rel: &str, cands: &[std::path::PathBuf]| {
        let p = ctx.app.join(rel);
        if p.is_file() {
            let sha = sha_file(&p).unwrap_or_default();
            say(format_args!(
                "  {label}{rel}  present  sha256={sha}  {}",
                lib_build_match(&sha, cands)
            ));
        } else {
            say(format_args!("  {label}{rel}  absent"));
        }
    };
    shown(
        "tool dll   ",
        TOOL_DLL,
        &[
            ctx.rs_lib.clone(),
            ctx.checkout.join("target/bridge-rs/win64/cs2_bridge.dll"),
            ctx.mcp_dir.join("target/bridge-rs/win64/cs2_bridge.dll"),
        ],
    );
    shown(
        "launcher   ",
        LAUNCH_EXE,
        std::slice::from_ref(&ctx.launch_exe),
    );
    shown("launcher   ", LOAD_DLL, std::slice::from_ref(&ctx.load_dll));
    let has_entry = fs::read(&ctx.man)
        .map(|m| String::from_utf8_lossy(&m).contains(text::ENTRY_MARKER))
        .unwrap_or(false);
    say(format_args!(
        "  manifest   {MANIFEST}  cs2_bridge entry {}",
        if has_entry { "present" } else { "absent" }
    ));
    for rel in MM_DESTS {
        let p = ctx.app.join(rel);
        let line = if p.is_file() {
            if rel.ends_with(".dll") {
                format!(
                    "{rel}  present  sha256={}",
                    sha_file(&p).unwrap_or_default()
                )
            } else {
                format!("{rel}  present")
            }
        } else if p.exists() {
            format!("{rel}  present (non-file)")
        } else {
            format!("{rel}  absent")
        };
        say(format_args!("  metamod    {line}"));
    }
    let rec = Records::load(bks(ctx), false)?;
    let gi = rec.gameinfo_kinds();
    if !gi.is_empty() {
        say(format_args!(
            "  gameinfo   the backups record un-migrated gameinfo-era edits ({}): install/ensure/uninstall refuse this tree",
            gi.join(", ")
        ));
    }
    if ctx.app.join(LEGACY_COPY).is_file() {
        say(format_args!("  legacy     {LEGACY_COPY}  present (installed by an OLD script; uninstall removes it)"));
    }
    Ok(0)
}

// ----- ensure ------------------------------------------------------------------------------------------

/// A copied artefact: current when byte-equal to the build WHEN THE BUILD EXISTS; no build + an
/// installed copy = current; no build + nothing installed = fatal ("build it first").
fn ensure_artefact(
    ctx: &Ctx,
    label: &str,
    rel: &str,
    build: &Path,
    issues: &mut bool,
    fatal: &mut Option<String>,
) {
    let dest = ctx.app.join(rel);
    if dest.is_file() {
        if !build.is_file() {
            ensure_report(
                "ok",
                format_args!("{label} {rel} (no build to compare: current by definition)"),
            );
        } else if fsx::same_bytes(&dest, build) {
            ensure_report("ok", format_args!("{label} {rel}"));
        } else {
            ensure_report(
                "stale:",
                format_args!("{label} {rel} differs from the built {}", build.display()),
            );
            *issues = true;
        }
        return;
    }
    if build.is_file() {
        ensure_report("missing", format_args!("{label} {rel}"));
    } else {
        ensure_report(
            "missing",
            format_args!(
                "{label} {rel} ({} is not built either - BUILD IT FIRST: `cargo bridge build`)",
                build.display()
            ),
        );
        *fatal = Some("build the artefact first (`cargo bridge build`)".into());
    }
    *issues = true;
}

pub fn ensure(ctx: &Ctx) -> R<u8> {
    let mut issues = false;
    let mut fatal: Option<String> = None;
    let all = bks(ctx);
    say("ensure (wine): reading artefacts (read-only; no container guard needed)");
    ensure_artefact(
        ctx,
        "tool DLL",
        TOOL_DLL,
        &ctx.rs_lib,
        &mut issues,
        &mut fatal,
    );
    ensure_artefact(
        ctx,
        "launcher exe",
        LAUNCH_EXE,
        &ctx.launch_exe,
        &mut issues,
        &mut fatal,
    );
    ensure_artefact(
        ctx,
        "mmload DLL",
        LOAD_DLL,
        &ctx.load_dll,
        &mut issues,
        &mut fatal,
    );
    // "current" = the file is what install's edit would produce from it, AND the Valve-owned
    // anchor the edit relies on (recorded in the oldest backup's ANCHORS; the default when no
    // backup has one) is still a line of it. Computed in memory: no write access needed.
    match fs::read_to_string(&ctx.man) {
        Ok(man) => {
            let mut wants: Vec<String> = all
                .first()
                .and_then(|b| fs::read_to_string(b.join("ANCHORS")).ok())
                .map(|a| {
                    a.lines()
                        .filter_map(|l| l.split_once('\t'))
                        .filter(|(k, w)| *k == "manifest" && !w.is_empty())
                        .map(|(_, w)| w.to_string())
                        .collect()
                })
                .unwrap_or_default();
            if wants.is_empty() {
                wants.push(text::DEFAULT_MANIFEST_ANCHOR.into());
            }
            let damaged = |anchor: &str| {
                format!(
                    "game file damaged: {} - missing anchor '{anchor}' (verify the game files; if Valve changed this line, update the manifest anchor of `cargo bridge install` (crates/cs2-bridge-tools/src/install/text.rs, apply_manifest_edit))",
                    ctx.man.display()
                )
            };
            if let Some(missing) = text::anchors_missing(&man, &wants) {
                ensure_report(
                    "fatal:",
                    format_args!("manifest {MANIFEST} - missing anchor '{missing}'"),
                );
                fatal = Some(damaged(&missing));
            } else {
                match text::apply_manifest_edit(&man) {
                    None => {
                        ensure_report(
                            "fatal:",
                            format_args!(
                                "manifest {MANIFEST} - no 'hammer' anchor to insert before"
                            ),
                        );
                        fatal = Some(damaged(text::DEFAULT_MANIFEST_ANCHOR));
                    }
                    Some(edited) if edited == man => {
                        ensure_report("ok", format_args!("manifest {MANIFEST} (cs2_bridge entry)"));
                    }
                    Some(_) => {
                        ensure_report(
                            "stale:",
                            format_args!("manifest {MANIFEST} is not what install's edit would produce (entry missing or the file truncated?)"),
                        );
                        issues = true;
                    }
                }
            }
        }
        Err(_) => {
            ensure_report(
                "fatal:",
                format_args!("manifest {MANIFEST} (whole file absent or unreadable)"),
            );
            fatal = Some(format!(
                "game file damaged: {} (verify the game files)",
                ctx.man.display()
            ));
            issues = true;
        }
    }
    for (rel, build) in [
        (MM_LOADER, "metamod/bin/win64/server.dll"),
        (MM_CORE, "metamod/bin/win64/metamod.2.cs2.dll"),
    ] {
        let dest = ctx.app.join(rel);
        if !dest.is_file() {
            ensure_report("missing", format_args!("metamod {rel}"));
            issues = true;
        } else if fsx::same_bytes(&dest, &ctx.mm_build.join(build)) {
            ensure_report("ok", format_args!("metamod {rel}"));
        } else {
            ensure_report(
                "stale:",
                format_args!("metamod {rel} differs from the metamod build"),
            );
            issues = true;
        }
    }
    if ctx.app.join(METAPLUGINS).is_file() {
        ensure_report(
            "ok",
            format_args!("metamod {METAPLUGINS} (present; NOT ours - content never compared)"),
        );
    } else {
        ensure_report("missing", format_args!("metamod {METAPLUGINS}"));
        issues = true;
    }
    match fs::read(ctx.app.join(VDF)) {
        Ok(bytes) if bytes == WINE_VDF.as_bytes() => {
            ensure_report("ok", format_args!("metamod {VDF}"))
        }
        Ok(_) => {
            ensure_report(
                "stale:",
                format_args!("metamod {VDF} differs from the expected vdf (file ../bin/win64/tools/cs2_bridge.dll)"),
            );
            issues = true;
        }
        Err(_) => {
            ensure_report("missing", format_args!("metamod {VDF}"));
            issues = true;
        }
    }
    let rec = Records::load(all, false)?;
    if let Some(msg) = records::gameinfo_era_problem(&rec, &ctx.backup_root) {
        ensure_report(
            "fatal:",
            format_args!("gameinfo-era records ({})", rec.gameinfo_kinds().join(", ")),
        );
        fatal = Some(msg);
    }
    ensure_repair(ctx, issues, fatal, || install(ctx))
}
