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
        // parents first, each recorded before it is created
        let parts: Vec<&str> = op.rel.split('/').collect();
        for i in 1..parts.len() {
            let drel = parts[..i].join("/");
            let d = under(root, &drel);
            if !d.exists() {
                m.entries.push(Entry { path: drel, kind: Kind::Dir, role: Role::Ours, before: None, after: None });
                store.save(&m)?;
                std::fs::create_dir(&d).with_context(|| format!("mkdir {}", d.display()))?;
            }
        }
        if target.is_dir() {
            bail!("{} is a directory; expected a file", target.display());
        }
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
        m.entries.push(Entry { path: op.rel.clone(), kind: Kind::File, role: Role::Ours, before, after: Some(after) });
        store.save(&m)?;
        write_atomic(&target, &bytes)?;
    }
    for rel in &plan.runtime {
        let target = under(root, rel);
        let before = match std::fs::read(&target) {
            Ok(b) => Some(before_of(store, &m, &target, &b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("read {}", target.display())),
        };
        m.entries.push(Entry { path: rel.clone(), kind: Kind::File, role: Role::Runtime, before, after: None });
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
}

impl Report {
    pub fn byte_for_byte(&self) -> bool {
        self.verify_failures.is_empty() && self.conflicts.is_empty()
    }
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

/// Restore the tree from the store's manifest, verify it, and archive the manifest. `Ok(None)`
/// when there is nothing to restore. `logs` receives the game-written files of the session.
pub fn restore(root: &Path, store: &Store, logs: &Path) -> anyhow::Result<Option<Report>> {
    let Some(mut m) = store.load()? else { return Ok(None) };
    m.state = State::Restoring;
    store.save(&m)?;
    let mut r = Report::default();
    let stamp = now_secs();
    for e in m.entries.iter().rev() {
        let target = under(root, &e.path);
        match (e.kind, e.role) {
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
        if r.conflicts.iter().any(|c| c.starts_with(&format!("{}:", e.path))) {
            continue;
        }
        let target = under(root, &e.path);
        r.checked += 1;
        match (&e.kind, &e.before) {
            (Kind::Dir, _) | (Kind::File, None) => {
                if target.exists() {
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
            if r.conflicts.is_empty() {
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
        let r = restore(&root, &store, &logs).unwrap().unwrap();
        assert!(r.byte_for_byte(), "{r:?}");
        assert_eq!(snapshot(&root), before);
        assert_eq!(r.saved_runtime.len(), 1);
        assert_eq!(std::fs::read(&r.saved_runtime[0]).unwrap(), b"session log");
        assert!(store.load().unwrap().is_none());
        assert!(!store.dir.join("backup").exists());
        assert!(restore(&root, &store, &logs).unwrap().is_none());
    }

    #[test]
    fn a_file_steam_replaced_is_left_alone_and_reported() {
        let (root, store, logs, plan) = setup("manifest-conflict");
        apply(&plan, &root, &store, &header()).unwrap();
        put(&root.join("game/bin/win64/steam_api64.dll"), b"valve api v2");
        let r = restore(&root, &store, &logs).unwrap().unwrap();
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
        let r = restore(&root, &store, &logs).unwrap().unwrap();
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
        let r = restore(&root, &store, &logs).unwrap().unwrap();
        assert!(r.byte_for_byte(), "{r:?}");
        assert_eq!(snapshot(&root), before_bad);
    }

    #[test]
    fn identical_files_are_not_recorded() {
        let (root, store, logs, mut plan) = setup("manifest-identical");
        plan.ops = vec![Op { rel: "game/bin/win64/steam_api64.dll".into(), source: Source::Text("valve api".into()) }];
        let m = apply(&plan, &root, &store, &header()).unwrap();
        assert!(m.entries.iter().all(|e| e.role == Role::Runtime));
        assert!(restore(&root, &store, &logs).unwrap().unwrap().byte_for_byte());
    }
}
