//! `--platform linux`: the `cs2-home` volume's game dir (client + dedicated share it). The
//! decisions run here; every read or write of the tree is a [`tree::Req`] inside `podman
//! unshare`. The backups live on the host (`<cs2-mcp>/target/bridge/backup-linux-<UTC>`).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::fsx::{self, die, say, OrDie, Stop, R};
use super::records::{self, backups_for_app, verify_backup_sha, Records};
use super::tree::{self, lrun, Req, BRIDGE_SO, MMLOAD_SO};
use super::{ensure_repair, guard, retire_backups, Ctx, LINUX_VOLUME};

/// Bottom-up, so `rmdir` (empty only) unwinds exactly what install created; `csgo` and
/// `csgo/addons` pre-exist in a real tree and simply stay. `_iconcache` is an empty dir the
/// engine created under `addons/metamod` while the gameinfo-era search path existed.
const LN_DIRS: [&str; 10] = [
    "csgo/addons/metamod/_iconcache",
    "csgo/addons/metamod/bin/linuxsteamrt64",
    "csgo/addons/metamod/bin/linux64",
    "csgo/addons/metamod/bin",
    "csgo/addons/metamod",
    "csgo/addons/cs2mcp/bin/linuxsteamrt64",
    "csgo/addons/cs2mcp/bin",
    "csgo/addons/cs2mcp",
    "csgo/addons",
    "csgo",
];

/// The game dir: `CS2_LINUX_GAME`, else the volume's (resolved through podman).
fn game_root(ctx: &Ctx) -> R<PathBuf> {
    if let Some(g) = &ctx.linux_game_override {
        return Ok(g.clone());
    }
    match guard::linux_vmp(ctx) {
        Some(vmp) => Ok(PathBuf::from(guard::volume_game_dir(&vmp))),
        None => Err(die(format_args!(
            "podman volume inspect {LINUX_VOLUME} failed - cannot resolve the Linux game dir (set CS2_LINUX_GAME for a test tree)"
        ))),
    }
}

fn bks(ctx: &Ctx, g: &Path) -> Vec<PathBuf> {
    backups_for_app(&ctx.backup_root, ctx.bk_prefix, &g.to_string_lossy())
}

/// A tree op whose failure is fatal: exit 99 (a fault point) propagates as such, anything else
/// becomes `what` failed.
fn lrun_or(ctx: &Ctx, req: &Req, what: &str) -> R<Value> {
    lrun(ctx, req).map_err(|code| {
        if code == 99 {
            Stop(99)
        } else {
            die(format_args!("{what} (rc {code}, see the output above)"))
        }
    })
}

/// The host builds that exist (a missing one: the installed copy stays).
fn built(p: &Path) -> Option<PathBuf> {
    p.is_file().then(|| p.to_path_buf())
}

fn preflight(ctx: &Ctx, g: &Path) -> R<()> {
    ctx.say_build_checkout(&ctx.so_lib);
    let probe = lrun_or(
        ctx,
        &Req::Probe {
            game: g.to_path_buf(),
            files: vec![BRIDGE_SO.into(), MMLOAD_SO.into()],
        },
        "linux preflight: reading the tree failed",
    )?;
    let installed = |i: usize| probe["files"][i].as_bool().unwrap_or(false);
    if ctx.so_lib.is_file() {
        ctx.export_gate(&ctx.so_lib, ".so")?;
    } else if installed(0) {
        say(format_args!(
            "no built .so at {} - the INSTALLED .so counts as current (only config is repaired)",
            ctx.so_lib.display()
        ));
    } else {
        return Err(die(format_args!(
            "no built .so at {} and none installed - BUILD IT FIRST (run `cargo bridge build`, or point CS2_BRIDGE_LINUX_SO at one)",
            ctx.so_lib.display()
        )));
    }
    // the LD_PRELOAD loader: not export-gated (it exports dlopen, not CreateInterface)
    if !ctx.mmload_so.is_file() {
        if installed(1) {
            say(format_args!(
                "no built mmload .so at {} - the INSTALLED one counts as current (only config is repaired)",
                ctx.mmload_so.display()
            ));
        } else {
            return Err(die(format_args!(
                "no mmload .so at {} and none installed - BUILD IT FIRST (run `cargo bridge build`, or point CS2_MMLOAD_SO at one)",
                ctx.mmload_so.display()
            )));
        }
    }
    for f in [
        "metamod/bin/linux64/server.so",
        "metamod/bin/linuxsteamrt64/libserver.so",
        "metamod/bin/linuxsteamrt64/metamod.2.cs2.so",
        "metamod/metaplugins.ini",
    ] {
        let p = ctx.mm_build_linux.join(f);
        if !p.is_file() {
            return Err(die(format_args!(
                "missing metamod build file: {} (crates/metamod/fetch-metamod.sh linux fetches it, or point CS2_MM_LINUX at a build)",
                p.display()
            )));
        }
    }
    // install only ADDS files, but it must never invent a game tree: csgo/ has to be there
    let missing = if !probe["game_dir"].as_bool().unwrap_or(false) {
        Some(format!("whole game dir {} absent", g.display()))
    } else if !probe["csgo_dir"].as_bool().unwrap_or(false) {
        Some(format!("{}/csgo absent", g.display()))
    } else {
        None
    };
    if let Some(m) = missing {
        say(format_args!("linux preflight: MISSING: {m}"));
        return Err(die(
            "refusing (game tree incomplete, see the MISSING line above)",
        ));
    }
    say("  preflight: game tree present (csgo/)");
    Ok(())
}

/// The backup, built in `<bk>.partial` (copies taken inside the namespace, so the volume's
/// files come out readable by the host user), renamed into place when complete. EDITS_WROTE is
/// empty: a linux install edits no game file.
fn backup(ctx: &Ctx, g: &Path, bk: &Path) -> R<()> {
    let mut partial = bk.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    fsx::set_partial(Some(partial.clone()));
    fs::create_dir_all(&partial).or_die(|| format!("cannot create {}", partial.display()))?;
    fs::write(partial.join("APP_TREE"), format!("{}\n", g.display()))
        .or_die(|| "cannot write APP_TREE".into())?;
    lrun_or(
        ctx,
        &Req::Backup {
            game: g.to_path_buf(),
            partial: partial.clone(),
            mm: ctx.mm_build_linux.clone(),
            lib: built(&ctx.so_lib),
            mmload: built(&ctx.mmload_so),
        },
        "linux backup failed inside the tree",
    )?;
    fs::write(partial.join("EDITS_WROTE"), "").or_die(|| "cannot write EDITS_WROTE".into())?;
    fsx::fault(ctx.fault_cfg(), "mid-backup")?;
    fs::rename(&partial, bk).or_die(|| format!("cannot finalise the backup {}", bk.display()))?;
    fsx::set_partial(None);
    Ok(())
}

fn write_list(bk: &Path, name: &str, lines: &[String]) -> R<()> {
    let dest = bk.join(name);
    let t = fsx::tmp_for(&dest).or_die(|| format!("cannot write {}", dest.display()))?;
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fsx::write_into(&t, body.as_bytes()).or_die(|| format!("cannot write {}", t.display()))?;
    fs::rename(&t, &dest).or_die(|| format!("cannot write {}", dest.display()))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub fn install(ctx: &Ctx) -> R<u8> {
    let g = game_root(ctx)?;
    guard::guard_live_game(ctx)?;
    preflight(ctx, &g)?;
    records::gameinfo_era_refusal(&Records::load(bks(ctx, &g), false)?, &ctx.backup_root)?;
    if ctx.dry {
        say(format_args!(
            "dry-run: would back up first under {}/{}-<UTC>; pre-existing (would-be-backed-up) files:",
            ctx.backup_root.display(),
            ctx.bk_prefix
        ));
        let _ = lrun(ctx, &Req::BackupList { game: g.clone() });
        say("dry-run: nothing below changes anything");
        say(format_args!(
            "DRY: mkdir csgo/addons/{{metamod,cs2mcp/bin/linuxsteamrt64}}; cp {}/metamod -> csgo/addons/metamod;",
            ctx.mm_build_linux.display()
        ));
        say(format_args!(
            "DRY: cp {} {} -> csgo/addons/cs2mcp/bin/linuxsteamrt64/",
            ctx.so_lib.display(),
            ctx.mmload_so.display()
        ));
        say(format_args!(
            "DRY: write {} (csgo/gameinfo.gi is not touched)",
            tree::VDF
        ));
        return Ok(0);
    }
    let bk = ctx.new_backup_dir();
    backup(ctx, &g, &bk)?;
    let out = lrun_or(
        ctx,
        &Req::Install {
            game: g.clone(),
            mm: ctx.mm_build_linux.clone(),
            lib: ctx.so_lib.clone(),
            mmload: ctx.mmload_so.clone(),
            fault: ctx.fault.clone(),
        },
        "linux install failed inside the tree",
    )?;
    write_list(&bk, "INSTALLED", &strings(&out["created"]))?;
    write_list(&bk, "WROTE_OVER", &strings(&out["wrote_over"]))?;
    say(
        "install done (linux: tool + loader + metamod, standard addons layout, gameinfo untouched)",
    );
    say("linux client launches need -insecure (Metamod refuses without -dedicated/-insecure); -dedicated covers the dedicated");
    Ok(0)
}

pub fn uninstall(ctx: &Ctx) -> R<u8> {
    let g = game_root(ctx)?;
    guard::guard_live_game(ctx)?;
    let all = bks(ctx, &g);
    let Some(oldest) = all.first().cloned() else {
        return Err(die(format_args!(
            "no {}-* directory under {} - refusing to uninstall without a backup",
            ctx.bk_prefix,
            ctx.backup_root.display()
        )));
    };
    // the records of EVERY backup of this tree, each contributing backup verified first
    let rec = Records::load(all, true)?;
    if rec.inst.is_empty() {
        return Err(die(format_args!(
            "no INSTALLED list in any backup of this tree under {} (the installs that wrote them did not complete) - refusing to uninstall: nothing would be removed",
            ctx.backup_root.display()
        )));
    }
    records::gameinfo_era_refusal(&rec, &ctx.backup_root)?;
    if ctx.dry {
        say(format_args!(
            "dry-run: would uninstall (linux) from {} (nothing changes)",
            oldest.display()
        ));
        return Ok(0);
    }
    verify_backup_sha(&oldest)?;
    if !oldest.join("EDITS_WROTE").is_file() {
        return Err(die(format_args!(
            "no EDITS_WROTE in {} (the install that wrote this backup did not complete) - refusing to uninstall: nothing would be removed",
            oldest.display()
        )));
    }
    // the INSTALLED union never deletes a PRE-INSTALL file (the restore gives the owner's back)
    let mut rels = Vec::new();
    for line in &rec.inst {
        let rel = line.split(' ').next().unwrap_or(line);
        if rec.pre_first(rel) {
            say(format_args!(
                "  (INSTALLED: {rel} pre-existed the first install - NOT deleted; the WROTE_OVER restore puts the owner's bytes back if install overwrote it)"
            ));
        } else {
            rels.push(line.clone());
        }
    }
    for kind in &rec.kinds {
        say(format_args!("  (unknown EDITS_WROTE line ignored: {kind})"));
    }
    say(format_args!(
        "restoring from {} (pre-first-install image)",
        oldest.display()
    ));
    let paths = rec.wrote_paths();
    lrun_or(
        ctx,
        &Req::Unwind {
            game: g.clone(),
            fault: ctx.fault.clone(),
            wro: rec.wro.clone(),
            paths,
            rels,
            dirs: LN_DIRS.iter().map(|d| d.to_string()).collect(),
        },
        "linux uninstall failed inside the tree",
    )?;
    retire_backups(ctx, &bks(ctx, &g));
    say("uninstall complete (linux: restore from the oldest backup + remove what install added)");
    Ok(0)
}

pub fn status(ctx: &Ctx) -> R<u8> {
    let g = game_root(ctx)?;
    say(format_args!(
        "linux game: {}  [{}]",
        g.display(),
        if ctx.linux_game_override.is_some() {
            "CS2_LINUX_GAME override (test tree)".to_string()
        } else {
            format!("volume {LINUX_VOLUME}")
        }
    ));
    ctx.say_build_checkout(&ctx.so_lib);
    let rs = ctx.checkout.join("target/bridge-rs/linuxsteamrt64");
    let canon = ctx.mcp_dir.join("target/bridge-rs/linuxsteamrt64");
    lrun_or(
        ctx,
        &Req::Status {
            game: g.clone(),
            lib_cands: vec![
                ctx.so_lib.clone(),
                rs.join("libcs2_bridge.so"),
                canon.join("libcs2_bridge.so"),
            ],
            mmload_cands: vec![
                ctx.mmload_so.clone(),
                rs.join("libcs2mcp_mmload.so"),
                canon.join("libcs2mcp_mmload.so"),
            ],
        },
        "linux status: reading the tree failed",
    )?;
    let rec = Records::load(bks(ctx, &g), false)?;
    let gi = rec.gameinfo_kinds();
    if !gi.is_empty() {
        say(format_args!(
            "gameinfo the backups record un-migrated gameinfo-era edits ({}): install/ensure/uninstall refuse this tree",
            gi.join(", ")
        ));
    }
    Ok(0)
}

pub fn ensure(ctx: &Ctx) -> R<u8> {
    let g = game_root(ctx)?;
    say("ensure (linux): reading artefacts (read-only; no container guard needed)");
    let out = lrun(
        ctx,
        &Req::Ensure {
            game: g.clone(),
            mm: ctx.mm_build_linux.clone(),
            lib: built(&ctx.so_lib),
            mmload: built(&ctx.mmload_so),
        },
    )
    .map_err(|code| die(format_args!("ensure: reading the tree failed (rc {code})")))?;
    let issues = out["issues"].as_bool().unwrap_or(true);
    let mut fatal = out["fatal"]
        .as_bool()
        .unwrap_or(false)
        .then(|| "build the artefact first (`cargo bridge build`)".to_string());
    let rec = Records::load(bks(ctx, &g), false)?;
    if let Some(msg) = records::gameinfo_era_problem(&rec, &ctx.backup_root) {
        super::ensure_report(
            "fatal:",
            format_args!("gameinfo-era records ({})", rec.gameinfo_kinds().join(", ")),
        );
        fatal = Some(msg);
    }
    ensure_repair(ctx, issues, fatal, || install(ctx))
}
