//! The payload's own integrity: a packaged launcher (`scripts/package.sh`, the MSI, the AppImage)
//! carries `SHA256SUMS` over every file of its package dir, and nothing is copied into a game
//! tree from a payload that does not match it. A missing, changed or extra file under `payload/`
//! (a partial download, an unpacked-over older version, a tampered file) is refused before
//! `prepare` touches anything; a development build without `SHA256SUMS` is reported as such.

use crate::findings::{Code, Finding, Severity};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SUMS: &str = "SHA256SUMS";

/// The outcome of [`verify`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum Check {
    /// every listed file matches and `payload/` holds nothing else
    Verified { sums: PathBuf, files: usize },
    /// no `SHA256SUMS` (a development tree): not checked
    Unverified { dir: PathBuf },
    /// refused: what is wrong
    Bad { sums: PathBuf, missing: Vec<String>, changed: Vec<String>, extra: Vec<String> },
}

impl Check {
    pub fn finding(&self) -> Option<Finding> {
        match self {
            Check::Verified { .. } => None,
            Check::Unverified { dir } => Some(Finding::new(
                Code::PayloadUnverified,
                Severity::Info,
                format!("{} has no {SUMS} (a development build): the payload is not verified", dir.display()),
            )),
            Check::Bad { sums, missing, changed, extra } => {
                let mut f = Finding::new(
                    Code::PayloadTampered,
                    Severity::Blocker,
                    format!("the launcher's payload does not match {}: refusing to install it", sums.display()),
                );
                f.paths.extend(missing.iter().map(|p| format!("missing: {p}")));
                f.paths.extend(changed.iter().map(|p| format!("changed: {p}")));
                f.paths.extend(extra.iter().map(|p| format!("not in {SUMS}: {p}")));
                Some(f.advice("download the release again (and check it against the release's SHA256SUMS), then reinstall or unpack it into an empty directory"))
            }
        }
    }
}

/// Parse `sha256sum` output (`<hex>  ./rel/path`, also `*` binary markers) into rel → hash.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some((hash, rest)) = line.split_once(' ') else { continue };
        let rel = rest.trim_start_matches([' ', '*']).trim_start_matches("./").replace('\\', "/");
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !rel.is_empty() {
            m.insert(rel, hash.to_ascii_lowercase());
        }
    }
    m
}

/// Check the package dir `dir` against its `SHA256SUMS`. Every listed file is hashed; files
/// under `payload/` that the list does not name are refused too (the Linux Metamod copy takes a
/// whole directory, so a stray file would be installed).
pub fn verify(dir: &Path) -> anyhow::Result<Check> {
    let sums = dir.join(SUMS);
    let text = match std::fs::read_to_string(&sums) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Check::Unverified { dir: dir.to_path_buf() }),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {}", sums.display()))),
    };
    let want = parse(&text);
    let (mut missing, mut changed, mut extra) = (Vec::new(), Vec::new(), Vec::new());
    for (rel, hash) in &want {
        let p = crate::manifest::under(dir, rel);
        match crate::gbe::sha256_file(&p) {
            Ok(h) if &h == hash => {}
            Ok(_) => changed.push(rel.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(rel.clone()),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("hash {}", p.display()))),
        }
    }
    let payload = dir.join("payload");
    if payload.is_dir() {
        crate::tree::walk(&payload, &mut |rel, _, md| {
            let full = format!("payload/{rel}");
            if !md.is_dir() && !want.contains_key(&full) {
                extra.push(full);
            }
            Ok(())
        })?;
    }
    if missing.is_empty() && changed.is_empty() && extra.is_empty() {
        Ok(Check::Verified { sums, files: want.len() })
    } else {
        Ok(Check::Bad { sums, missing, changed, extra })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{put, scratch};

    fn package(name: &str) -> PathBuf {
        let d = scratch(name);
        put(&d.join("cs2mcp-native"), b"exe");
        put(&d.join("payload/bridge-rs/linuxsteamrt64/libcs2_bridge.so"), b"bridge");
        put(&d.join("payload/gbe/linux64/steamclient.so"), b"gbe");
        let mut s = String::new();
        for rel in ["cs2mcp-native", "payload/bridge-rs/linuxsteamrt64/libcs2_bridge.so", "payload/gbe/linux64/steamclient.so"] {
            s += &format!("{}  ./{rel}\n", crate::gbe::sha256_file(&d.join(rel)).unwrap());
        }
        std::fs::write(d.join(SUMS), s).unwrap();
        d
    }

    #[test]
    fn a_matching_payload_verifies_and_a_dev_tree_is_unverified() {
        let d = package("payload-ok");
        assert!(matches!(verify(&d).unwrap(), Check::Verified { files: 3, .. }));
        let dev = scratch("payload-dev");
        let c = verify(&dev).unwrap();
        assert!(matches!(c, Check::Unverified { .. }));
        assert_eq!(c.finding().unwrap().severity, Severity::Info);
    }

    #[test]
    fn missing_changed_and_extra_files_are_refused() {
        let d = package("payload-bad");
        put(&d.join("payload/gbe/linux64/steamclient.so"), b"tampered");
        std::fs::remove_file(d.join("payload/bridge-rs/linuxsteamrt64/libcs2_bridge.so")).unwrap();
        put(&d.join("payload/metamod/linux/addons/metamod/evil.so"), b"x");
        match verify(&d).unwrap() {
            Check::Bad { missing, changed, extra, .. } => {
                assert_eq!(missing, ["payload/bridge-rs/linuxsteamrt64/libcs2_bridge.so"]);
                assert_eq!(changed, ["payload/gbe/linux64/steamclient.so"]);
                assert_eq!(extra, ["payload/metamod/linux/addons/metamod/evil.so"]);
            }
            c => panic!("{c:?}"),
        }
        let f = verify(&d).unwrap().finding().unwrap();
        assert_eq!((f.code, f.severity), (Code::PayloadTampered, Severity::Blocker));
    }

    #[test]
    fn sha256sum_formats_parse() {
        let h = "a".repeat(64);
        let m = parse(&format!("{h}  ./a/b\n{h} *c\r\nnot a line\n"));
        assert_eq!(m.keys().collect::<Vec<_>>(), ["a/b", "c"]);
    }
}
