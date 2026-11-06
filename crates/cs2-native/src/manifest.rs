//! The in-place strategy: apply a [`Plan`] to the user's tree with a write-ahead manifest and
//! byte backups, and restore + verify it (README "Windows (and Linux `--strategy in-place`)").
//!
//! Every change is recorded (and the record flushed) **before** the file is written, so a crash
//! at any point leaves a manifest that covers what is on disk. Restore never overwrites bytes it
//! did not write: a file someone else changed since (a Steam update, "Verify integrity") is left
//! alone and reported.

use crate::plan::{Plan, Source, SteamMode};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const FORMAT: &str = "cs2mcp-native/manifest/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// entries are being written; the tree may be half-prepared (restore handles it)
    Applying,
    Applied,
    /// a restore started and did not finish
    Restoring,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    /// a symbolic link on the way to one of our files (setup_gbe.sh's `steam_settings` link out of
    /// the install): removed for the session, so nothing is written through it, and created again
    /// by restore with its old target and mtime ([`Link`])
    Link,
}

/// A symbolic link as it was before us.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub target: String,
    /// the link's own modification time (ns since the epoch, lstat)
    #[serde(default)]
    pub mtime_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// we write it
    Ours,
    /// the game writes it during our session; the user's copy is backed up and put back
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Before {
    pub sha256: String,
    pub size: u64,
    /// relative to the state dir
    pub backup: String,
    /// modification time (ns since the epoch), put back with the bytes
    #[serde(default)]
    pub mtime_ns: Option<u64>,
    /// Unix permission bits, put back with the bytes
    #[serde(default)]
    pub mode: Option<u32>,
}

fn file_times(p: &Path) -> (Option<u64>, Option<u32>) {
    let Ok(md) = std::fs::metadata(p) else { return (None, None) };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64);
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(md.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = None;
    (mtime, mode)
}

fn before_of(store: &Store, m: &Manifest, target: &Path, bytes: &[u8]) -> anyhow::Result<Before> {
    let backup = store.backup(m, bytes)?;
    let meta = hash_bytes(bytes);
    let (mtime_ns, mode) = file_times(target);
    Ok(Before { sha256: meta.sha256, size: meta.size, backup, mtime_ns, mode })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub role: Role,
    /// the user's bytes, or `None` when the path did not exist
    pub before: Option<Before>,
    /// what we wrote (`None` for directories and runtime files)
    pub after: Option<Meta>,
    /// [`Kind::Link`]: the link to put back
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: String,
    pub install_root: String,
    pub build: Option<String>,
    pub steam: SteamMode,
    pub tools: bool,
    pub state: State,
    pub created_at: u64,
    pub entries: Vec<Entry>,
}

/// What the manifest is about (the header fields).
#[derive(Debug, Clone)]
pub struct Header {
    pub build: Option<String>,
    pub steam: SteamMode,
    pub tools: bool,
}

/// The manifest + backup store of one install (`<state dir>`).
#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

/// `rel` (forward slashes) under `root`, built component by component so the separators are
/// the platform's.
pub fn under(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').filter(|c| !c.is_empty()).fold(root.to_path_buf(), |p, c| p.join(c))
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SHA-256 + size of a file; `None` when it does not exist.
pub fn hash_file(p: &Path) -> anyhow::Result<Option<Meta>> {
    let mut f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("open {}", p.display())),
    };
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf).with_context(|| format!("read {}", p.display()))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        h.update(&buf[..n]);
    }
    Ok(Some(Meta { sha256: hex(&h.finalize()), size }))
}

pub fn hash_bytes(b: &[u8]) -> Meta {
    Meta { sha256: hex(&Sha256::digest(b)), size: b.len() as u64 }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// How long a file the game had open may stay locked after its process is gone (Windows tears
/// a killed process down asynchronously: a mapped DLL refuses a rename over it for a moment).
pub const IN_USE_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Run `op`, retrying while it fails because another process still holds the file (Windows
/// sharing violation 32 / access denied 5), until `timeout`. `NotFound` counts as done for
/// removals, so callers decide that themselves. `target` is the file the operation removes or
/// replaces: access denied on a **read-only** `target` is not a lock that goes away, so it is
/// returned at once instead of after `timeout`.
pub fn retry_in_use(
    target: &Path,
    timeout: std::time::Duration,
    mut op: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let until = std::time::Instant::now() + timeout;
    let read_only = || std::fs::symlink_metadata(target).is_ok_and(|m| m.is_file() && m.permissions().readonly());
    loop {
        match op() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && e.raw_os_error() != Some(32) && read_only() => {
                return Err(std::io::Error::new(e.kind(), format!("{e} ({} is read-only)", target.display())));
            }
            Err(e) if std::time::Instant::now() < until && (e.raw_os_error() == Some(32) || e.kind() == std::io::ErrorKind::PermissionDenied) => {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => return Err(e),
        }
    }
}

fn remove_file_retrying(path: &Path) -> std::io::Result<()> {
    retry_in_use(path, IN_USE_GRACE, || match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    })
}

/// Write `bytes` to `dst` through a temp file next to it, flushed, then renamed over `dst`
/// (replace-existing on both platforms).
pub fn write_atomic(dst: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut name = dst.file_name().context("no file name")?.to_os_string();
    name.push(".cs2mcp-tmp");
    let tmp = dst.with_file_name(name);
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().ok();
    }
    retry_in_use(dst, IN_USE_GRACE, || std::fs::rename(&tmp, dst)).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename over {} (is the game running from this tree?)", dst.display())
    })
}

impl Store {
    /// The [`crate::tree::scan`] of the install taken by [`apply`].
    pub fn listing_path(&self) -> PathBuf {
        self.dir.join("tree-before.json")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }
    pub fn load(&self) -> anyhow::Result<Option<Manifest>> {
        match std::fs::read(self.manifest_path()) {
            Ok(b) => {
                let m: Manifest = serde_json::from_slice(&b)
                    .with_context(|| format!("parse {}", self.manifest_path().display()))?;
                if m.format != FORMAT {
                    bail!("{}: unknown format {:?}", self.manifest_path().display(), m.format);
                }
                Ok(Some(m))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read manifest"),
        }
    }
    fn save(&self, m: &Manifest) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_atomic(&self.manifest_path(), &serde_json::to_vec_pretty(m)?)
    }
    fn backup(&self, m: &Manifest, bytes: &[u8]) -> anyhow::Result<String> {
        let n = m.entries.iter().filter(|e| e.before.is_some()).count() + 1;
        let rel = format!("backup/{n:04}");
        let p = under(&self.dir, &rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        write_atomic(&p, bytes)?;
        let got = hash_file(&p)?.context("backup vanished")?;
        if got != hash_bytes(bytes) {
            bail!("backup {} does not read back what was written", p.display());
        }
        Ok(rel)
    }
}

/// The bytes an op writes, given the tree it is applied to (an edit reads the current file).
pub fn op_bytes(root: &Path, src: &Source, rel: &str) -> anyhow::Result<Vec<u8>> {
    match src {
        Source::Copy(p) => std::fs::read(p).with_context(|| format!("read artefact {}", p.display())),
        Source::Text(t) => Ok(t.as_bytes().to_vec()),
        Source::EditSdkEngineTools => {
            let p = under(root, rel);
            let t = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
            crate::layout::sdkenginetools_edit(&t).map(String::into_bytes).with_context(|| {
                format!(
                    "game file damaged: {} has no `m_Name = \"hammer\"` entry to insert before (verify the game files in Steam)",
                    p.display()
                )
            })
        }
    }
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// A directory link to take out for the session: Unix only (a Windows junction or symlink in the
/// install is refused; no layout of ours makes one).
fn link_of(p: &Path) -> anyhow::Result<Link> {
    if cfg!(not(unix)) {
        bail!("{} is a link or junction; cs2mcp-native does not write through one (remove it, or `cs2mcp-native cleanup --yes`)", p.display());
    }
    let target = std::fs::read_link(p).with_context(|| format!("read the link {}", p.display()))?;
    let mtime_ns = std::fs::symlink_metadata(p).ok().and_then(|m| crate::tree::mtime_ns(&m));
    Ok(Link { target: target.to_string_lossy().into_owned(), mtime_ns })
}

/// A file we write (or the game writes) must not be a link: writing would replace the link with a
/// file, and restore would put back the bytes it pointed at, not the link.
fn refuse_link(p: &Path) -> anyhow::Result<()> {
    if is_link(p) {
        bail!("{} is a symbolic link; cs2mcp-native does not replace one (remove it, or `cs2mcp-native cleanup --yes`)", p.display());
    }
    Ok(())
}

/// Create the link `l` at `p` again, with its old mtime.
fn put_back_link(p: &Path, l: &Link) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::os::unix::fs::symlink(&l.target, p).with_context(|| format!("symlink {} -> {}", p.display(), l.target))?;
        if let Some(ns) = l.mtime_ns {
            let c = std::ffi::CString::new(p.as_os_str().as_bytes())?;
            let t = libc::timespec { tv_sec: (ns / 1_000_000_000) as libc::time_t, tv_nsec: (ns % 1_000_000_000) as libc::c_long };
            // [atime, mtime] of the link itself
            // SAFETY: a valid C path and two timespecs.
            if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [t, t].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| format!("set the mtime of the link {}", p.display()));
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    bail!("cannot create the link {} -> {} here", p.display(), l.target)
}

/// Is `rel` under a [`Kind::Link`] entry of `m` whose link is in place again? Then the path leads
/// out of the install, to what the link points at: never touched or checked from here.
fn beyond_link(m: &Manifest, root: &Path, rel: &str) -> bool {
    m.entries.iter().any(|l| {
        l.kind == Kind::Link
            && rel.starts_with(&format!("{}/", l.path))
            && l.link.as_ref().is_some_and(|k| link_matches(&under(root, &l.path), k))
    })
}

/// Is `p` the link `l` (same target)?
fn link_matches(p: &Path, l: &Link) -> bool {
    is_link(p) && std::fs::read_link(p).is_ok_and(|t| t.to_string_lossy() == l.target)
}

/// Apply `plan` to `root`. Refuses when the store still holds an unfinished manifest (restore it
/// first; `up` does).
pub fn apply(plan: &Plan, root: &Path, store: &Store, header: &Header) -> anyhow::Result<Manifest> {
    if let Some(m) = store.load()? {
        bail!(
            "{} still records a prepared tree (state {:?}); restore it first (`cs2mcp-native uninstall`)",
            store.manifest_path().display(),
            m.state
        );
    }
    let mut m = Manifest {
        format: FORMAT.into(),
        install_root: root.display().to_string(),
        build: header.build.clone(),
        steam: header.steam,
        tools: header.tools,
        state: State::Applying,
        created_at: now_secs(),
        entries: Vec::new(),
    };
    store.save(&m)?;
    // what the tree looked like before us and the game: restore reports the game's own writes
    let listing = crate::tree::scan(root)?;
    std::fs::write(store.listing_path(), serde_json::to_vec(&listing)?)?;
    for op in &plan.ops {
        let target = under(root, &op.rel);
        let bytes = op_bytes(root, &op.source, &op.rel)?;
        let after = hash_bytes(&bytes);
        // parents first, each recorded before it is created; a link among them is taken out first
        // (written through, it would put our files wherever it points, out of the install)
        let parts: Vec<&str> = op.rel.split('/').collect();
        for i in 1..parts.len() {
            let drel = parts[..i].join("/");
            let d = under(root, &drel);
            if is_link(&d) {
                let link = link_of(&d)?;
                m.entries.push(Entry { path: drel.clone(), kind: Kind::Link, role: Role::Ours, before: None, after: None, link: Some(link) });
                store.save(&m)?;
                std::fs::remove_file(&d).with_context(|| format!("remove the link {}", d.display()))?;
            }
            if !d.exists() {
                m.entries.push(Entry { path: drel, kind: Kind::Dir, role: Role::Ours, before: None, after: None, link: None });
                store.save(&m)?;
                std::fs::create_dir(&d).with_context(|| format!("mkdir {}", d.display()))?;
            }
        }
        if target.is_dir() {
            bail!("{} is a directory; expected a file", target.display());
        }
        refuse_link(&target)?;
        let before = match std::fs::read(&target) {
            Ok(b) => {
                if hash_bytes(&b) == after {
                    continue; // already exactly these bytes: nothing to change or restore
                }
                Some(before_of(store, &m, &target, &b)?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("read {}", target.display())),
        };
        m.entries.push(Entry { path: op.rel.clone(), kind: Kind::File, role: Role::Ours, before, after: Some(after), link: None });
        store.save(&m)?;
        write_atomic(&target, &bytes)?;
    }
    for rel in &plan.runtime {
        let target = under(root, rel);
        refuse_link(&target)?;
        let before = match std::fs::read(&target) {
            Ok(b) => Some(before_of(store, &m, &target, &b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("read {}", target.display())),
        };
        m.entries.push(Entry { path: rel.clone(), kind: Kind::File, role: Role::Runtime, before, after: None, link: None });
    }
    m.state = State::Applied;
    store.save(&m)?;
    Ok(m)
}

/// The outcome of [`restore`].
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// user bytes put back
    pub restored: Vec<String>,
    /// paths we created, removed again
    pub removed: Vec<String>,
    /// already in the user's state
    pub unchanged: Vec<String>,
    /// changed by someone else after we wrote it: left alone
    pub conflicts: Vec<String>,
    /// game-written files of our session, moved to the logs dir
    pub saved_runtime: Vec<String>,
    /// verification failures (empty = byte-for-byte)
    pub verify_failures: Vec<String>,
    /// entries verified
    pub checked: usize,
    /// where the finished manifest went
    pub archived: Option<String>,
    /// what the game wrote in the install outside our entries (`added: …` / `changed: …` /
    /// `removed: …` by size or mtime against the listing taken at prepare). Not undone: Workshop
    /// Tools write settings, thumbnail and bake caches into the install on every run.
    pub game_written: Vec<String>,
    /// entries whose backup was missing or damaged and that the user declared lost
    /// (`--forget-missing-backups`): our bytes were removed where they were still ours, the
    /// user's bytes could not be put back
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lost: Vec<String>,
    /// the manifest's state before this restore (`applying`: a prepare that crashed mid-way)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_state: Option<State>,
}

impl Report {
    pub fn byte_for_byte(&self) -> bool {
        self.verify_failures.is_empty() && self.conflicts.is_empty() && self.lost.is_empty()
    }

    /// The restore's outcome as findings: conflicts (Steam changed our files: a decision, since
    /// the tree is not the user's old one), lost backups, verify failures.
    pub fn findings(&self) -> Vec<crate::findings::Finding> {
        use crate::findings::{Code, Finding, Severity, VERIFY_ADVICE};
        let mut v = Vec::new();
        if !self.conflicts.is_empty() {
            v.push(
                Finding::new(
                    Code::Conflicts,
                    Severity::Decision,
                    format!(
                        "{} path(s) changed by someone else since we wrote them (a Steam update or \"Verify integrity\" during the session); left as they are",
                        self.conflicts.len()
                    ),
                )
                .paths(self.conflicts.iter().cloned())
                .advice("Steam's newer bytes were kept; the user's older copies are kept in the archived manifest's backup/ dir")
                .advice(VERIFY_ADVICE)
                .advice("--accept-conflicts (GUI: Accept) acknowledges this"),
            );
        }
        if !self.lost.is_empty() {
            v.push(
                Finding::new(Code::LostBackups, Severity::Warn, format!("{} file(s) recorded as lost: their backups were gone", self.lost.len()))
                    .paths(self.lost.iter().cloned())
                    .advice(VERIFY_ADVICE),
            );
        }
        if !self.verify_failures.is_empty() {
            v.push(
                Finding::new(Code::VerifyFailed, Severity::Blocker, "the restored tree does not verify; the manifest stays recorded")
                    .paths(self.verify_failures.iter().cloned())
                    .advice("fix or remove the files listed, then `cs2mcp-native uninstall` (GUI: Repair)")
                    .advice(VERIFY_ADVICE),
            );
        }
        v
    }
}

/// How [`restore`] treats backups that are gone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreOpts {
    /// restore everything else; for an entry without its backup, remove our bytes where they are
    /// still ours and record the path as lost (`--forget-missing-backups`)
    pub forget_missing_backups: bool,
}

/// `(entry path, reason)` for every backup `m` names that is missing or no longer hashes to its
/// record.
pub fn missing_backups(store: &Store, m: &Manifest) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for e in &m.entries {
        let Some(b) = &e.before else { continue };
        let p = under(&store.dir, &b.backup);
        match hash_file(&p) {
            Ok(Some(h)) if h.sha256 == b.sha256 => {}
            Ok(Some(_)) => v.push((e.path.clone(), format!("{} is damaged (sha256 differs from the record)", b.backup))),
            Ok(None) => v.push((e.path.clone(), format!("{} is missing", b.backup))),
            Err(err) => v.push((e.path.clone(), format!("{}: {err:#}", b.backup))),
        }
    }
    v
}

fn missing_backups_stop(store: &Store, missing: &[(String, String)]) -> crate::findings::Stopped {
    use crate::findings::{Code, Finding, Severity, Stopped, VERIFY_ADVICE};
    let f = Finding::new(
        Code::MissingBackups,
        Severity::Blocker,
        format!(
            "{} records a prepared tree, but the backups of {} file(s) are gone: those files cannot be put back. Nothing was changed.",
            store.manifest_path().display(),
            missing.len()
        ),
    )
    .paths(missing.iter().map(|(p, why)| format!("{p}: {why}")))
    .advice("if you still have the state dir's backup/ files elsewhere, put them back and retry")
    .advice("otherwise: --forget-missing-backups (GUI: Mark as lost, after confirming) restores everything else, removes our bytes from these paths and records them as lost")
    .advice(VERIFY_ADVICE);
    Stopped { message: f.title.clone(), findings: vec![f] }
}

fn put_back(store: &Store, root: &Path, e: &Entry, b: &Before) -> anyhow::Result<()> {
    let bytes = std::fs::read(under(&store.dir, &b.backup)).with_context(|| format!("read backup {}", b.backup))?;
    if hash_bytes(&bytes).sha256 != b.sha256 {
        bail!("backup {} for {} is damaged (sha256 differs from the record)", b.backup, e.path);
    }
    let target = under(root, &e.path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&target, &bytes)?;
    #[cfg(unix)]
    if let Some(mode) = b.mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
    }
    if let Some(ns) = b.mtime_ns {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ns);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .and_then(|f| f.set_modified(t))
            .with_context(|| format!("set the mtime of {}", target.display()))?;
    }
    Ok(())
}

/// Remove our `sdkenginetools.txt` entry in place when that gives exactly the bytes `before`
/// records (sha256); `false` (file unchanged) otherwise.
fn unedit_matches(target: &Path, before: Option<&String>) -> bool {
    let Some(want) = before else { return false };
    let Ok(text) = std::fs::read_to_string(target) else { return false };
    let Some(orig) = crate::layout::sdkenginetools_unedit(&text) else { return false };
    if &hash_bytes(orig.as_bytes()).sha256 != want {
        return false;
    }
    write_atomic(target, orig.as_bytes()).is_ok()
}

/// Restore the tree from the store's manifest, verify it, and archive the manifest. `Ok(None)`
/// when there is nothing to restore. `logs` receives the game-written files of the session.
pub fn restore(root: &Path, store: &Store, logs: &Path, opts: RestoreOpts) -> anyhow::Result<Option<Report>> {
    let Some(mut m) = store.load()? else { return Ok(None) };
    // before anything is touched: a restore that would stop half-way on a missing backup leaves
    // a tree worse than the one it found
    let missing = missing_backups(store, &m);
    if !missing.is_empty() && !opts.forget_missing_backups {
        return Err(missing_backups_stop(store, &missing).into());
    }
    let lost: Vec<&str> = missing.iter().map(|(p, _)| p.as_str()).collect();
    let mut r = Report { previous_state: Some(m.state), ..Report::default() };
    m.state = State::Restoring;
    store.save(&m)?;
    let stamp = now_secs();
    for e in m.entries.iter().rev() {
        let target = under(root, &e.path);
        if beyond_link(&m, root, &e.path) {
            r.unchanged.push(e.path.clone());
            continue;
        }
        match (e.kind, e.role) {
            (Kind::Link, _) => {
                let Some(l) = &e.link else { bail!("{}: a link entry without its target", e.path) };
                if link_matches(&target, l) {
                    r.unchanged.push(e.path.clone());
                } else if std::fs::symlink_metadata(&target).is_err() {
                    put_back_link(&target, l)?;
                    r.restored.push(e.path.clone());
                } else {
                    r.conflicts.push(format!("{}: something else is where the link {} was; left in place", e.path, l.target));
                }
            }
            (Kind::Dir, _) => match std::fs::read_dir(&target) {
                Ok(mut it) => {
                    if it.next().is_none() {
                        std::fs::remove_dir(&target).with_context(|| format!("rmdir {}", target.display()))?;
                        r.removed.push(e.path.clone());
                    } else {
                        r.conflicts.push(format!("{}: directory we created is not empty; left in place", e.path));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => r.unchanged.push(e.path.clone()),
                Err(err) => return Err(err).with_context(|| format!("read_dir {}", target.display())),
            },
            (Kind::File, Role::Ours) if lost.contains(&e.path.as_str()) => {
                let cur = hash_file(&target)?;
                let after = e.after.as_ref().map(|a| &a.sha256);
                let b = e.before.as_ref().map(|b| &b.sha256);
                match cur {
                    Some(c) if Some(&c.sha256) == b => r.unchanged.push(e.path.clone()),
                    // our one edit can be undone without the backup: the result is checked
                    // against the recorded sha256 of the user's bytes, like a put_back
                    Some(c) if Some(&c.sha256) == after && e.path.ends_with("sdkenginetools.txt") && unedit_matches(&target, b) => {
                        r.restored.push(e.path.clone());
                    }
                    Some(c) if Some(&c.sha256) == after => {
                        remove_file_retrying(&target).with_context(|| format!("remove {}", target.display()))?;
                        r.lost.push(format!("{}: our copy removed; the original could not be put back", e.path));
                    }
                    None => r.lost.push(format!("{}: missing; the original could not be put back", e.path)),
                    Some(_) => r.conflicts.push(format!(
                        "{}: changed by someone else since we wrote it (a Steam update or Verify?); left as is",
                        e.path
                    )),
                }
            }
            (Kind::File, Role::Runtime) if lost.contains(&e.path.as_str()) => {
                r.lost.push(format!("{}: the game's copy of this session left in place; the earlier one could not be put back", e.path));
            }
            (Kind::File, Role::Ours) => {
                let cur = hash_file(&target)?;
                let after = e.after.as_ref().map(|a| &a.sha256);
                match (&e.before, cur) {
                    (None, None) => r.unchanged.push(e.path.clone()),
                    (None, Some(c)) if Some(&c.sha256) == after => {
                        remove_file_retrying(&target).with_context(|| format!("remove {}", target.display()))?;
                        r.removed.push(e.path.clone());
                    }
                    (None, Some(_)) => r.conflicts.push(format!("{}: changed since we created it; left in place", e.path)),
                    (Some(b), Some(c)) if c.sha256 == b.sha256 => r.unchanged.push(e.path.clone()),
                    (Some(b), Some(c)) if Some(&c.sha256) == after => {
                        put_back(store, root, e, b)?;
                        r.restored.push(e.path.clone());
                    }
                    (Some(b), None) => {
                        put_back(store, root, e, b)?;
                        r.restored.push(e.path.clone());
                    }
                    (Some(_), Some(_)) => r.conflicts.push(format!(
                        "{}: changed by someone else since we wrote it (a Steam update or Verify?); left as is",
                        e.path
                    )),
                }
            }
            (Kind::File, Role::Runtime) => {
                let cur = hash_file(&target)?;
                let same = matches!((&e.before, &cur), (Some(b), Some(c)) if b.sha256 == c.sha256);
                if let (Some(_), false) = (&cur, same) {
                    std::fs::create_dir_all(logs)?;
                    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let dst = logs.join(format!("{stamp}-{name}"));
                    std::fs::copy(&target, &dst).with_context(|| format!("save {} to {}", target.display(), dst.display()))?;
                    r.saved_runtime.push(dst.display().to_string());
                    if let Err(err) = remove_file_retrying(&target) {
                        r.conflicts.push(format!("{}: still in use ({err}); saved a copy, left in place", e.path));
                        continue;
                    }
                }
                match (&e.before, same) {
                    (Some(b), true) => {
                        // rewritten with the same bytes (the game's steam_appid.txt): the mtime too
                        if let Some(ns) = b.mtime_ns {
                            let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ns);
                            let _ = std::fs::OpenOptions::new().write(true).open(&target).and_then(|f| f.set_modified(t));
                        }
                        r.unchanged.push(e.path.clone())
                    }
                    (_, true) => r.unchanged.push(e.path.clone()),
                    (Some(b), false) => {
                        put_back(store, root, e, b)?;
                        r.restored.push(e.path.clone());
                    }
                    (None, false) => {
                        if cur.is_some() {
                            r.removed.push(e.path.clone());
                        } else {
                            r.unchanged.push(e.path.clone());
                        }
                    }
                }
            }
        }
    }
    // verify: every entry not in conflict is back to the user's state
    for e in &m.entries {
        let tagged = |list: &[String]| list.iter().any(|c| c.starts_with(&format!("{}:", e.path)));
        if tagged(&r.conflicts) || tagged(&r.lost) {
            continue;
        }
        if beyond_link(&m, root, &e.path) {
            continue;
        }
        let target = under(root, &e.path);
        r.checked += 1;
        match (&e.kind, &e.before) {
            (Kind::Link, _) => {
                if !e.link.as_ref().is_some_and(|l| link_matches(&target, l)) {
                    r.verify_failures.push(format!("{}: the link is not back", e.path));
                }
            }
            (Kind::Dir, _) | (Kind::File, None) => {
                // not followed: a link put back at the same path is that link's entry's business
                let relinked = m.entries.iter().any(|x| x.kind == Kind::Link && x.path == e.path) && is_link(&target);
                if std::fs::symlink_metadata(&target).is_ok() && !relinked {
                    r.verify_failures.push(format!("{}: still exists", e.path));
                }
            }
            (Kind::File, Some(b)) => match hash_file(&target)? {
                Some(c) if c.sha256 == b.sha256 && c.size == b.size => {}
                Some(c) => r.verify_failures.push(format!("{}: sha256 {} != recorded {}", e.path, c.sha256, b.sha256)),
                None => r.verify_failures.push(format!("{}: missing", e.path)),
            },
        }
    }
    if let Ok(b) = std::fs::read(store.listing_path()) {
        let before: crate::tree::Listing = serde_json::from_slice(&b).context("tree-before.json")?;
        let after = crate::tree::scan(root)?;
        let ours = |p: &str| m.entries.iter().any(|e| e.path == p);
        r.game_written = crate::tree::changes(&before, &after, &ours);
    }
    if r.verify_failures.is_empty() {
        let hist = store.dir.join("history").join(format!("{stamp}"));
        std::fs::create_dir_all(&hist)?;
        std::fs::rename(store.manifest_path(), hist.join("manifest.json"))?;
        if store.listing_path().exists() {
            std::fs::rename(store.listing_path(), hist.join("tree-before.json"))?;
        }
        let bk = store.dir.join("backup");
        if bk.exists() {
            if r.conflicts.is_empty() && r.lost.is_empty() {
                std::fs::remove_dir_all(&bk)?;
            } else {
                std::fs::rename(&bk, hist.join("backup"))?; // keep the user's old bytes
            }
        }
        std::fs::write(hist.join("report.json"), serde_json::to_vec_pretty(&r)?)?;
        r.archived = Some(hist.display().to_string());
    }
    Ok(Some(r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Op;
    use crate::testutil::{put, scratch};

    const MAN: &str = "\t\t{\n\t\t\tm_Name = \"hammer\"\n\t\t},\n";

    /// Access denied on a read-only target is returned at once, not after the 15 s grace
    /// (FIXME 7); on a writable target it is retried like a sharing violation.
    #[test]
    fn a_read_only_target_is_not_retried() {
        let base = scratch("retry-read-only");
        let f = base.join("ro.bin");
        put(&f, b"x");
        let mut perms = std::fs::metadata(&f).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&f, perms.clone()).unwrap();
        let denied = || Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let t0 = std::time::Instant::now();
        let err = retry_in_use(&f, std::time::Duration::from_secs(10), denied).unwrap_err();
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        assert!(err.to_string().contains("read-only"), "{err}");
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&f, perms).unwrap();
        let mut calls = 0;
        let r = retry_in_use(&f, std::time::Duration::from_millis(600), || {
            calls += 1;
            if calls < 2 { denied() } else { Ok(()) }
        });
        assert!(r.is_ok());
        assert_eq!(calls, 2);
    }

    fn setup(name: &str) -> (PathBuf, Store, PathBuf, Plan) {
        let base = scratch(name);
        let root = base.join("tree");
        put(&root.join("game/bin/win64/steam_api64.dll"), b"valve api");
        put(&root.join("game/bin/sdkenginetools.txt"), MAN.as_bytes());
        put(&root.join("game/csgo/console.log"), b"user's old log");
        put(&base.join("art/gbe.dll"), b"emulator");
        for f in ["game/bin/win64/steam_api64.dll", "game/bin/sdkenginetools.txt", "game/csgo/console.log"] {
            let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
            std::fs::OpenOptions::new().write(true).open(root.join(f)).unwrap().set_modified(old).unwrap();
        }
        let plan = Plan {
            ops: vec![
                Op { rel: "game/bin/win64/steam_api64.dll".into(), source: Source::Copy(base.join("art/gbe.dll")) },
                Op { rel: "game/bin/win64/steam_settings/configs.main.ini".into(), source: Source::Text("x".into()) },
                Op { rel: "game/bin/sdkenginetools.txt".into(), source: Source::EditSdkEngineTools },
                Op { rel: "game/csgo/addons/metamod/cs2mcp.vdf".into(), source: Source::Text("vdf".into()) },
            ],
            runtime: vec!["game/csgo/console.log".into()],
        };
        (root, Store { dir: base.join("state") }, base.join("logs"), plan)
    }

    /// (path, sha256 + mtime for files): the same view `tree-hash` takes
    fn snapshot(root: &Path) -> Vec<(String, Option<String>)> {
        let mut v = Vec::new();
        fn walk(base: &Path, d: &Path, v: &mut Vec<(String, Option<String>)>) {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                if p.is_dir() {
                    v.push((rel, None));
                    walk(base, &p, v);
                } else {
                    let mt = std::fs::metadata(&p).unwrap().modified().unwrap();
                    v.push((rel, Some(format!("{} {mt:?}", hash_file(&p).unwrap().unwrap().sha256))));
                }
            }
        }
        walk(root, root, &mut v);
        v.sort();
        v
    }

    fn header() -> Header {
        Header { build: Some("1".into()), steam: SteamMode::Gbe, tools: true }
    }

    #[test]
    fn apply_then_restore_is_byte_for_byte_and_saves_the_session_log() {
        let (root, store, logs, plan) = setup("manifest-roundtrip");
        let before = snapshot(&root);
        let m = apply(&plan, &root, &store, &header()).unwrap();
        assert_eq!(m.state, State::Applied);
        assert_eq!(std::fs::read(root.join("game/bin/win64/steam_api64.dll")).unwrap(), b"emulator");
        assert!(std::fs::read_to_string(root.join("game/bin/sdkenginetools.txt")).unwrap().contains("cs2_bridge"));
        // dirs we created are recorded before their files
        let paths: Vec<_> = m.entries.iter().map(|e| (e.path.as_str(), e.kind)).collect();
        assert!(paths.contains(&("game/bin/win64/steam_settings", Kind::Dir)));
        assert!(paths.contains(&("game/csgo/addons", Kind::Dir)));
        // the game writes its log during the session
        put(&root.join("game/csgo/console.log"), b"session log");
        let r = restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap();
        assert!(r.byte_for_byte(), "{r:?}");
        assert_eq!(snapshot(&root), before);
        assert_eq!(r.saved_runtime.len(), 1);
        assert_eq!(std::fs::read(&r.saved_runtime[0]).unwrap(), b"session log");
        assert!(store.load().unwrap().is_none());
        assert!(!store.dir.join("backup").exists());
        assert!(restore(&root, &store, &logs, RestoreOpts::default()).unwrap().is_none());
    }

    /// setup_gbe.sh's `steam_settings` link out of the install: nothing is written through it, and
    /// it is back afterwards (same target, same mtime), also after a crash right after it went.
    #[cfg(unix)]
    #[test]
    fn a_linked_settings_dir_is_taken_out_for_the_session_and_put_back() {
        let (root, store, logs, plan) = setup("manifest-link");
        let home = root.parent().unwrap().join("home-settings");
        put(&home.join("configs.main.ini"), b"the image's settings");
        let link = root.join("game/bin/win64/steam_settings");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let lmt = |p: &Path| std::fs::symlink_metadata(p).unwrap().modified().unwrap();
        put_back_link(&root.join("probe"), &Link { target: "x".into(), mtime_ns: Some(1_600_000_000_000_000_000) }).unwrap();
        assert_eq!(lmt(&root.join("probe")), old);
        std::fs::remove_file(root.join("probe")).unwrap();
        let before = snapshot(&root);
        let link_mtime = lmt(&link);
        let m = apply(&plan, &root, &store, &header()).unwrap();
        let k: Vec<_> = m.entries.iter().filter(|e| e.path == "game/bin/win64/steam_settings").map(|e| e.kind).collect();
        assert_eq!(k, [Kind::Link, Kind::Dir]);
        assert!(!is_link(&link) && link.join("configs.main.ini").is_file());
        assert_eq!(std::fs::read(home.join("configs.main.ini")).unwrap(), b"the image's settings");
        let r = restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap();
        assert!(r.byte_for_byte(), "{r:?}");
        assert!(r.restored.contains(&"game/bin/win64/steam_settings".to_string()));
        assert_eq!(std::fs::read_link(&link).unwrap(), home);
        assert_eq!(lmt(&link), link_mtime);
        assert_eq!(snapshot(&root), before);
        assert_eq!(std::fs::read(home.join("configs.main.ini")).unwrap(), b"the image's settings");

        // a crash right after the link went: restore creates it again
        let mut m = Manifest { state: State::Applying, entries: Vec::new(), ..m };
        m.entries.push(Entry { path: "game/bin/win64/steam_settings".into(), kind: Kind::Link, role: Role::Ours, before: None, after: None, link: link_of(&link).ok() });
        store.save(&m).unwrap();
        std::fs::remove_file(&link).unwrap();
        let r = restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap();
        assert!(r.byte_for_byte(), "{r:?}");
        assert_eq!(std::fs::read_link(&link).unwrap(), home);

        // a linked file of ours is refused before anything is written
        let api = root.join("game/bin/win64/steam_api64.dll");
        std::fs::rename(&api, root.join("api.real")).unwrap();
        std::os::unix::fs::symlink(root.join("api.real"), &api).unwrap();
        let err = apply(&plan, &root, &store, &header()).unwrap_err();
        assert!(err.to_string().contains("is a symbolic link"), "{err}");
        assert_eq!(std::fs::read(root.join("api.real")).unwrap(), b"valve api");
    }

    #[test]
    fn a_file_steam_replaced_is_left_alone_and_reported() {
        let (root, store, logs, plan) = setup("manifest-conflict");
        apply(&plan, &root, &store, &header()).unwrap();
        put(&root.join("game/bin/win64/steam_api64.dll"), b"valve api v2");
        let r = restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap();
        assert!(!r.byte_for_byte());
        assert_eq!(r.conflicts.len(), 1, "{r:?}");
        assert!(r.verify_failures.is_empty(), "{r:?}");
        assert_eq!(std::fs::read(root.join("game/bin/win64/steam_api64.dll")).unwrap(), b"valve api v2");
        // the user's old bytes are kept with the archived manifest
        let hist = PathBuf::from(r.archived.unwrap());
        assert!(hist.join("backup").is_dir());
    }

    #[test]
    fn a_deleted_original_is_put_back_and_a_second_apply_refuses_until_restored() {
        let (root, store, logs, plan) = setup("manifest-deleted");
        apply(&plan, &root, &store, &header()).unwrap();
        assert!(apply(&plan, &root, &store, &header()).unwrap_err().to_string().contains("restore it first"));
        std::fs::remove_file(root.join("game/bin/sdkenginetools.txt")).unwrap();
        let r = restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap();
        assert!(r.byte_for_byte(), "{r:?}");
        assert_eq!(std::fs::read_to_string(root.join("game/bin/sdkenginetools.txt")).unwrap(), MAN);
    }

    #[test]
    fn a_crash_mid_apply_still_restores() {
        let (root, store, logs, plan) = setup("manifest-crash");
        // the third op fails (anchor gone): the first two are on disk and recorded
        put(&root.join("game/bin/sdkenginetools.txt"), b"no anchor");
        let before_bad = snapshot(&root);
        assert!(apply(&plan, &root, &store, &header()).is_err());
        assert_eq!(store.load().unwrap().unwrap().state, State::Applying);
        let r = restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap();
        assert!(r.byte_for_byte(), "{r:?}");
        assert_eq!(snapshot(&root), before_bad);
    }

    #[test]
    fn missing_backups_stop_before_anything_changes_and_can_be_declared_lost() {
        use crate::findings::{Code, Stopped};
        let (root, store, logs, plan) = setup("manifest-missing-backup");
        apply(&plan, &root, &store, &header()).unwrap();
        let applied = snapshot(&root);
        // the state dir lost its backups (a cleanup tool, a sync, a hand)
        std::fs::remove_dir_all(store.dir.join("backup")).unwrap();
        let e = restore(&root, &store, &logs, RestoreOpts::default()).unwrap_err();
        let stop = e.downcast_ref::<Stopped>().expect("a findings stop");
        assert_eq!(stop.findings[0].code, Code::MissingBackups);
        assert_eq!(stop.findings[0].paths.len(), 3, "{:?}", stop.findings[0].paths);
        assert_eq!(snapshot(&root), applied, "nothing was touched");
        assert_eq!(store.load().unwrap().unwrap().state, State::Applied);

        let r = restore(&root, &store, &logs, RestoreOpts { forget_missing_backups: true }).unwrap().unwrap();
        // Valve's DLL: our copy removed, the original lost (Verify integrity brings it back)
        assert!(!root.join("game/bin/win64/steam_api64.dll").exists());
        assert!(r.lost.iter().any(|l| l.starts_with("game/bin/win64/steam_api64.dll:")), "{r:?}");
        // the one edit is undone without its backup, and checked against the recorded hash
        assert!(r.restored.contains(&"game/bin/sdkenginetools.txt".to_string()), "{r:?}");
        assert_eq!(std::fs::read_to_string(root.join("game/bin/sdkenginetools.txt")).unwrap(), MAN);
        // what we created is removed as always
        assert!(!root.join("game/csgo/addons/metamod/cs2mcp.vdf").exists());
        assert!(r.verify_failures.is_empty(), "{r:?}");
        assert!(!r.byte_for_byte());
        assert!(store.load().unwrap().is_none(), "archived");
        assert!(r.findings().iter().any(|f| f.code == Code::LostBackups));
    }

    #[test]
    fn a_steam_verify_mid_session_is_a_decision_with_the_verify_advice() {
        use crate::findings::{Code, Severity};
        let (root, store, logs, plan) = setup("manifest-verify-mid");
        apply(&plan, &root, &store, &header()).unwrap();
        // Steam rewrote both the DLL and the manifest file while the session ran
        put(&root.join("game/bin/win64/steam_api64.dll"), b"valve api, new build");
        put(&root.join("game/bin/sdkenginetools.txt"), b"new tools list");
        let r = restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap();
        let f = r.findings();
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!((f[0].code, f[0].severity), (Code::Conflicts, Severity::Decision));
        assert_eq!(f[0].paths.len(), 2);
        assert!(f[0].advice.iter().any(|a| a.contains("Verify integrity")));
        assert_eq!(r.previous_state, Some(State::Applied));
    }

    #[test]
    fn identical_files_are_not_recorded() {
        let (root, store, logs, mut plan) = setup("manifest-identical");
        plan.ops = vec![Op { rel: "game/bin/win64/steam_api64.dll".into(), source: Source::Text("valve api".into()) }];
        let m = apply(&plan, &root, &store, &header()).unwrap();
        assert!(m.entries.iter().all(|e| e.role == Role::Runtime));
        assert!(restore(&root, &store, &logs, RestoreOpts::default()).unwrap().unwrap().byte_for_byte());
    }
}
