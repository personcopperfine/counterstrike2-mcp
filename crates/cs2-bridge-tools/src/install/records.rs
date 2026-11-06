//! The backups and what they record: the ONE reader for everything that needs to know what an
//! earlier install of ours did (uninstall on both platforms, ensure's anchor source, status).
//!
//! A backup is `<cs2-mcp>/target/bridge/backup-rs-<UTC>/` (Wine) or `backup-linux-<UTC>/`
//! (Linux), built as `<dir>.partial` and renamed into place when complete. The format is the
//! one the shell tool (`bridge/install-rs.sh`, in history) wrote, kept byte-compatible so a tree
//! it installed is checked and uninstalled by this command:
//!
//! * `APP_TREE`: the tree key (Wine: the app dir as spelled, `dirname $CS2_GAME`; Linux: the
//!   game dir), one line. Only the backups of the tree at hand are read.
//! * `SHA256SUMS`: `sha256sum` lines (`<hex>  <rel>`) of the copies in the backup (the files
//!   that existed before that install and that it would write). Verified before a backup's
//!   lists are used; the oldest backup's list is the pre-first-install file set.
//! * `INSTALLED`: the files that install CREATED (`<rel>`, or `<rel> <sha>` for a created
//!   `metaplugins.ini`: deleted only while still byte-identical). Written as the PLANNED list
//!   before the backup is finalised, rewritten as the actual list at the end.
//! * `WROTE_OVER`: the pre-existing files install overwrote, `<rel> <sha-of-what-we-wrote>`
//!   (planned, then actual). Uninstall restores a path only while its current sha is one we
//!   wrote ([`Records::chain_src_for`]).
//! * `EDITS_WROTE`: `manifest:cs2_bridge` when that install added the manifest entry. Backups of
//!   the gameinfo-era layout also hold `gameinfo:<line>` kinds and `reverted:<kind>` lines that
//!   cancel them (the gameinfo-era migration wrote those); see [`Records::gameinfo_kinds`].
//! * `ANCHORS`: `<kind>\t<stripped line>`: the Valve-owned manifest line the edit anchors on, as
//!   it read at install time.

use std::fs;
use std::path::{Path, PathBuf};

use super::fsx::{die, read_trimmed, say, sha_file, R};

/// This tree's live backups, oldest first (UTC stamps sort lexicographically). `*.partial` dirs
/// are skipped: an unfinished backup is not a backup.
pub fn backups_for_app(root: &Path, prefix: &str, key: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let lead = format!("{prefix}-");
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&lead) && !name.ends_with(".partial")
        })
        .map(|e| e.path())
        .filter(|b| read_trimmed(&b.join("APP_TREE")).as_deref() == Some(key))
        .collect();
    out.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    out
}

/// The backup's SHA256SUMS must be present and intact before its lists or copies are used. An
/// empty list is valid (nothing of ours pre-existed that install).
pub fn verify_backup_sha(bk: &Path) -> R<()> {
    let sums = bk.join("SHA256SUMS");
    let Ok(text) = fs::read_to_string(&sums) else {
        return Err(die(format_args!(
            "no SHA256SUMS in {} - cannot trust this backup, refusing",
            bk.display()
        )));
    };
    for line in text.lines().filter(|l| !l.is_empty()) {
        let (hash, rel) = split_sum(line);
        if sha_file(&bk.join(rel)).ok().as_deref() != Some(hash) {
            return Err(die(format_args!(
                "backup SHA256SUMS mismatch in {} (it was modified since install; refusing)",
                bk.display()
            )));
        }
    }
    Ok(())
}

/// A `sha256sum` line: `<hex>  <path>` (text mode) or `<hex> *<path>` (binary mode).
fn split_sum(line: &str) -> (&str, &str) {
    if let Some((h, p)) = line.split_once("  ") {
        return (h, p);
    }
    if let Some((h, p)) = line.split_once(" *") {
        return (h, p);
    }
    (line, line)
}

/// One WROTE_OVER entry: the path, the sha of the bytes that install wrote, and its backup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Wrote {
    pub rel: String,
    pub sha: String,
    pub bk: PathBuf,
}

/// Why [`Records::chain_src_for`] found nothing to restore from.
#[derive(Debug, PartialEq, Eq)]
pub enum NoSource {
    /// no install of ours wrote the current bytes: changed after our last write, leave it
    Changed,
    /// the backup copy to restore from is gone
    Missing,
}

/// Everything this tree's backups record.
#[derive(Debug, Default)]
pub struct Records {
    /// live backups, oldest first
    pub bks: Vec<PathBuf>,
    /// every INSTALLED line of every backup
    pub inst: Vec<String>,
    /// the files the OLDEST backup's SHA256SUMS lists (the pre-first-install image)
    pub pre: Vec<String>,
    /// every WROTE_OVER entry, oldest backup first
    pub wro: Vec<Wrote>,
    /// the union of every backup's EDITS_WROTE kinds, minus the `reverted:` ones (deduplicated)
    pub kinds: Vec<String>,
}

impl Records {
    /// Read the records of `bks`. With `verify`, every backup that contributes a list is
    /// SHA256SUMS-checked first (a mismatch refuses) and a missing list is warned about (an
    /// old-format backup); without it the read is silent and never fails (the read-only checks).
    pub fn load(bks: Vec<PathBuf>, verify: bool) -> R<Records> {
        let mut rec = Records::default();
        let mut all_kinds: Vec<String> = Vec::new();
        let mut cancelled: Vec<String> = Vec::new();
        for bk in &bks {
            match fs::read_to_string(bk.join("INSTALLED")) {
                Ok(text) => {
                    if verify {
                        verify_backup_sha(bk)?;
                    }
                    rec.inst.extend(text.lines().filter(|l| !l.is_empty()).map(String::from));
                }
                Err(_) if verify => say(format_args!(
                    "  WARN: no INSTALLED list in {} (an old-format backup) - what IT created stays",
                    bk.display()
                )),
                Err(_) => {}
            }
            match fs::read_to_string(bk.join("WROTE_OVER")) {
                Ok(text) => {
                    if verify {
                        verify_backup_sha(bk)?;
                    }
                    for line in text.lines().filter(|l| !l.is_empty()) {
                        let (rel, sha) = line.split_once(' ').unwrap_or((line, line));
                        rec.wro.push(Wrote { rel: rel.into(), sha: sha.into(), bk: bk.clone() });
                    }
                }
                Err(_) if verify => say(format_args!(
                    "  WARN: no WROTE_OVER list in {} (an old-format backup) - what IT overwrote cannot be restored",
                    bk.display()
                )),
                Err(_) => {}
            }
            if let Ok(text) = fs::read_to_string(bk.join("EDITS_WROTE")) {
                for k in text.lines().filter(|l| !l.is_empty()) {
                    match k.strip_prefix("reverted:") {
                        Some(c) => cancelled.push(c.into()),
                        None => all_kinds.push(k.into()),
                    }
                }
            }
        }
        for k in all_kinds {
            if !cancelled.contains(&k) && !rec.kinds.contains(&k) {
                rec.kinds.push(k);
            }
        }
        if let Some(oldest) = bks.first() {
            if let Ok(text) = fs::read_to_string(oldest.join("SHA256SUMS")) {
                rec.pre.extend(
                    text.lines()
                        .filter(|l| !l.is_empty())
                        .map(|l| split_sum(l).1.to_string()),
                );
            }
        }
        rec.bks = bks;
        Ok(rec)
    }

    /// `rel` existed before the FIRST install (the oldest backup lists it): the owner's file,
    /// never deleted by uninstall (the WROTE_OVER restore gives the owner's bytes back instead).
    pub fn pre_first(&self, rel: &str) -> bool {
        self.pre.iter().any(|p| p == rel)
    }

    /// The gameinfo-era edits some backup of this tree recorded and no later backup cancelled.
    /// The gameinfo-free layout makes no such edit; the migration that removed those lines from
    /// old trees is gone from this command, so a tree that still carries them is refused (see
    /// [`gameinfo_era_refusal`]).
    pub fn gameinfo_kinds(&self) -> Vec<&str> {
        self.kinds
            .iter()
            .filter(|k| k.starts_with("gameinfo:"))
            .map(String::as_str)
            .collect()
    }

    /// The backup copy to restore `rel` from, given its current sha (the chain rule). Install may
    /// write a path more than once (a rebuild between installs makes ensure reinstall it), so the
    /// oldest entry does not describe our LAST write: start from the NEWEST entry whose recorded
    /// sha is the current sha, take that backup's copy, and while that copy is itself what the
    /// next-older entry's install wrote, step to the older backup's copy. The last copy reached is
    /// the bytes that existed before we FIRST wrote the path.
    pub fn chain_src_for(&self, rel: &str, cur: &str) -> Result<PathBuf, NoSource> {
        let mut mi = self
            .wro
            .iter()
            .rposition(|w| w.rel == rel && w.sha == cur)
            .ok_or(NoSource::Changed)?;
        loop {
            let copy = self.wro[mi].bk.join(rel);
            if !copy.is_file() {
                break;
            }
            let Some(older) = self.wro[..mi].iter().rposition(|w| w.rel == rel) else {
                break;
            };
            match sha_file(&copy) {
                Ok(copy_sha) if copy_sha == self.wro[older].sha => mi = older,
                _ => break,
            }
        }
        let copy = self.wro[mi].bk.join(rel);
        if copy.is_file() {
            Ok(copy)
        } else {
            Err(NoSource::Missing)
        }
    }

    /// The WROTE_OVER paths, each once, in first-appearance order.
    pub fn wrote_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in &self.wro {
            if !out.contains(&w.rel) {
                out.push(w.rel.clone());
            }
        }
        out
    }
}

/// Why a tree is refused (before anything is touched) when its backups record gameinfo-era
/// edits that were never migrated away. Those lines (`Game metamod` / `Game
/// csgo/addons/metamod` plus `Write csgo`) would put a second Metamod in play next to the
/// gameinfo-free layout, and removing them was the job of the migration, which is gone from this
/// command. `None`: nothing of the kind recorded.
pub fn gameinfo_era_problem(rec: &Records, backup_root: &Path) -> Option<String> {
    let kinds = rec.gameinfo_kinds();
    if kinds.is_empty() {
        return None;
    }
    Some(format!(
        "the backups of this tree under {} record gameinfo-era edits that were never migrated away ({}); \
         this command no longer migrates that layout. Run the last tool that does \
         (`git show 8cdea42:bridge/install-rs.sh`, with its bridge/lib-root.sh and check-exports.sh) \
         to migrate or uninstall, then retry",
        backup_root.display(),
        kinds.join(", ")
    ))
}

/// [`gameinfo_era_problem`] as a refusal (install, uninstall).
pub fn gameinfo_era_refusal(rec: &Records, backup_root: &Path) -> R<()> {
    match gameinfo_era_problem(rec, backup_root) {
        Some(msg) => Err(die(msg)),
        None => Ok(()),
    }
}
