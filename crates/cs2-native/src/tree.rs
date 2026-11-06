//! A cheap listing of the install tree (size + mtime per file, no hashing), taken when the tree is
//! prepared in place and compared on restore. It reports what the **game itself** wrote during the
//! session (Workshop Tools rewrite settings, caches and bake output inside the install), which the
//! manifest does not cover and the launcher does not undo.

use std::collections::BTreeMap;
use std::path::Path;

/// `rel path` (`/`-separated, dirs end in `/`) → `[size, mtime_ns]` (`null` for dirs).
pub type Listing = BTreeMap<String, Option<(u64, Option<u64>)>>;

/// Walk `root`, calling `f(rel, path, metadata)` for every entry (dirs first, rel ends in `/`).
/// Links are not followed: a link is an entry of its own (its `lstat` metadata).
pub fn walk(
    root: &Path,
    f: &mut dyn FnMut(&str, &Path, &std::fs::Metadata) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    fn go(
        base: &Path,
        d: &Path,
        f: &mut dyn FnMut(&str, &Path, &std::fs::Metadata) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let rd = match std::fs::read_dir(d) {
            Ok(rd) => rd,
            // a dir the game removed between listing and descent
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("read_dir {}", d.display()))),
        };
        for e in rd {
            let e = e?;
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            let rel = p.strip_prefix(base)?.to_string_lossy().replace('\\', "/");
            if md.is_dir() {
                f(&format!("{rel}/"), &p, &md)?;
                go(base, &p, f)?;
            } else {
                f(&rel, &p, &md)?;
            }
        }
        Ok(())
    }
    go(root, root, f)
}

pub fn mtime_ns(md: &std::fs::Metadata) -> Option<u64> {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
}

pub fn scan(root: &Path) -> anyhow::Result<Listing> {
    let mut m = Listing::new();
    walk(root, &mut |rel, _, md| {
        m.insert(rel.to_string(), (!md.is_dir()).then(|| (md.len(), mtime_ns(md))));
        Ok(())
    })?;
    Ok(m)
}

/// `added: …` / `removed: …` / `changed: …` (size or mtime) between two listings, except the
/// paths `skip` claims (the manifest's own entries).
pub fn changes(before: &Listing, after: &Listing, skip: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in after {
        if skip(k.trim_end_matches('/')) {
            continue;
        }
        match before.get(k) {
            None => out.push(format!("added: {k}")),
            Some(o) if o != v => out.push(format!("changed: {k}")),
            _ => {}
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) && !skip(k.trim_end_matches('/')) {
            out.push(format!("removed: {k}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_skip_ours_and_see_the_games() {
        let base = crate::testutil::scratch("tree");
        std::fs::create_dir_all(base.join("game/csgo")).unwrap();
        std::fs::write(base.join("game/csgo/a.txt"), "a").unwrap();
        std::fs::write(base.join("game/csgo/gone.txt"), "g").unwrap();
        let before = scan(&base).unwrap();
        std::fs::write(base.join("game/csgo/a.txt"), "aa").unwrap();
        std::fs::remove_file(base.join("game/csgo/gone.txt")).unwrap();
        std::fs::write(base.join("game/csgo/ours.dll"), "x").unwrap();
        std::fs::create_dir_all(base.join("game/csgo/cache")).unwrap();
        let after = scan(&base).unwrap();
        let c = changes(&before, &after, &|p| p == "game/csgo/ours.dll");
        assert_eq!(
            c,
            ["changed: game/csgo/a.txt", "added: game/csgo/cache/", "removed: game/csgo/gone.txt"]
        );
    }
}
