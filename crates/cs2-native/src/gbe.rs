//! `cs2mcp-native fetch-gbe`: gbe_fork's own Linux release (`libsteam_api.so`, `steamclient.so`)
//! into `<state root>/gbe/linux64/`, for a launcher without a staged `payload/gbe` (no repo, no
//! podman). The pin is `scripts/stage-gbe.sh`'s (the tests hold the two equal): tag, commit and
//! the asset's SHA-256, checked before anything is extracted. Shells out to `curl` and `tar`
//! (bzip2) rather than adding HTTP and archive crates; the download only happens when the user
//! runs the command.

use crate::install::Os;
use crate::layout;
use anyhow::{bail, Context};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// gbe_fork's release for the Dockerfile's `GBE_REF` (bump with stage-gbe.sh's pin block).
pub const REL_TAG: &str = "release-2026_09_16_2";
pub const REL_COMMIT: &str = "8479ddbc096f7d8a418ec28f463c829b77f66911";
pub const LINUX_ASSET: &str = "emu-linux-release.tar.bz2";
pub const LINUX_SHA256: &str = "59b284b3a22513dabde29f2fe4e256e0e76ac154738d13bf46da064c78b9a12e";
/// the x64 regular build inside the archive
const LINUX_MEMBER_DIR: &str = "release/regular/x64";
pub const LINUX_FILES: [&str; 2] = ["libsteam_api.so", "steamclient.so"];

pub fn release_url(asset: &str) -> String {
    format!("https://github.com/Detanup01/gbe_fork/releases/download/{REL_TAG}/{asset}")
}

/// `<state root>/gbe`: where `fetch-gbe` puts the libraries and `Artifacts::resolve` looks second.
pub fn fetched_dir(state_root: &Path) -> PathBuf {
    state_root.join("gbe")
}

/// `<state root>/gbe-release/<tag>`: the downloaded archive, kept for reruns.
pub fn cache_dir(state_root: &Path) -> PathBuf {
    state_root.join("gbe-release").join(REL_TAG)
}

/// Does `dir` hold the gbe files `os` installs (the layout stage-gbe.sh writes)?
pub fn has_files(dir: &Path, os: Os) -> bool {
    match os {
        Os::Windows => dir.join("win64/steam_api64.dll").is_file(),
        Os::Linux => LINUX_FILES.iter().all(|f| dir.join("linux64").join(f).is_file()),
    }
}

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(crate::manifest::hex(&h.finalize()))
}

pub fn check_sha256(p: &Path, want: &str) -> anyhow::Result<()> {
    let got = sha256_file(p).with_context(|| format!("reading {}", p.display()))?;
    if !got.eq_ignore_ascii_case(want) {
        bail!("{} has SHA-256 {got}, the pin is {want}; refusing it", p.display());
    }
    Ok(())
}

fn run(cmd: &mut Command, what: &str) -> anyhow::Result<()> {
    let st = cmd
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("running {what} (fetch-gbe needs `curl` and `tar` with bzip2 on PATH)"))?;
    if !st.success() {
        bail!("{what} failed ({st})");
    }
    Ok(())
}

pub struct FetchOpts<'a> {
    /// the gbe dir to fill (`linux64/` + `GBE_REF.txt`)
    pub dest: &'a Path,
    /// where the archive is downloaded to and reused from
    pub cache: &'a Path,
    /// an archive the user already has (offline); still hash-checked
    pub archive: Option<&'a Path>,
}

/// Download (or reuse) the pinned Linux release, check it, install its two libraries.
pub fn fetch_linux(o: &FetchOpts<'_>, say: impl Fn(String)) -> anyhow::Result<Value> {
    let archive = match o.archive {
        Some(a) => {
            check_sha256(a, LINUX_SHA256)?;
            a.to_path_buf()
        }
        None => {
            let cached = o.cache.join(LINUX_ASSET);
            if cached.is_file() && check_sha256(&cached, LINUX_SHA256).is_ok() {
                say(format!("using the cached {}", cached.display()));
            } else {
                std::fs::create_dir_all(o.cache).with_context(|| format!("creating {}", o.cache.display()))?;
                let part = o.cache.join(format!("{LINUX_ASSET}.part"));
                let url = release_url(LINUX_ASSET);
                say(format!("downloading {url}"));
                run(Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&part).arg(&url), "curl")?;
                check_sha256(&part, LINUX_SHA256)?;
                std::fs::rename(&part, &cached)?;
            }
            cached
        }
    };
    let files = install_from_archive(&archive, o.dest)?;
    std::fs::write(
        o.dest.join("GBE_REF.txt"),
        format!("GBE_REF={REL_COMMIT}\nlinux64: gbe_fork {REL_TAG} {LINUX_ASSET} (sha256 {LINUX_SHA256})\n"),
    )?;
    Ok(json!({
        "gbeDir": o.dest, "tag": REL_TAG, "commit": REL_COMMIT,
        "asset": LINUX_ASSET, "assetSha256": LINUX_SHA256, "archive": archive, "files": files,
    }))
}

/// Extract the two libraries into a scratch dir under `dest`, refuse anything that is not the
/// emulator, then move them into `dest/linux64/`. The archive's hash is the caller's to check.
fn install_from_archive(archive: &Path, dest: &Path) -> anyhow::Result<Value> {
    let work = dest.join(".fetch-gbe");
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let members: Vec<String> = LINUX_FILES.iter().map(|f| format!("{LINUX_MEMBER_DIR}/{f}")).collect();
    let r = (|| {
        run(Command::new("tar").arg("-xjf").arg(archive).arg("-C").arg(&work).args(&members), "tar")?;
        for f in LINUX_FILES {
            let p = work.join(LINUX_MEMBER_DIR).join(f);
            let bytes = std::fs::read(&p).with_context(|| format!("{f} is not in {}", archive.display()))?;
            if !layout::is_gbe_library(&bytes) {
                bail!("{f} in {} is not the gbe emulator; refusing it", archive.display());
            }
        }
        let out = dest.join("linux64");
        std::fs::create_dir_all(&out)?;
        let mut files = serde_json::Map::new();
        for f in LINUX_FILES {
            let dst = out.join(f);
            std::fs::rename(work.join(LINUX_MEMBER_DIR).join(f), &dst)?;
            files.insert(f.to_string(), sha256_file(&dst)?.into());
        }
        Ok(Value::Object(files))
    })();
    let _ = std::fs::remove_dir_all(&work);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{put, scratch};

    /// stage-gbe.sh and this module carry one pin; a bump that misses one side fails here.
    #[test]
    fn the_pin_is_stage_gbe_sh_s() {
        let sh = include_str!("../scripts/stage-gbe.sh");
        for line in [
            format!("rel_tag={REL_TAG}\n"),
            format!("rel_commit={REL_COMMIT}\n"),
            format!("linux_asset={LINUX_ASSET}\n"),
            format!("linux_sha256={LINUX_SHA256}\n"),
        ] {
            assert!(sh.contains(&line), "stage-gbe.sh has no `{}`", line.trim());
        }
        assert!(sh.contains("release/regular/x64/libsteam_api.so release/regular/x64/steamclient.so"));
        assert_eq!(
            release_url(LINUX_ASSET),
            "https://github.com/Detanup01/gbe_fork/releases/download/release-2026_09_16_2/emu-linux-release.tar.bz2"
        );
    }

    #[test]
    fn the_hash_check_refuses_a_mismatch() {
        let d = scratch("gbe-hash");
        let f = d.join("a");
        put(&f, b"abc");
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        check_sha256(&f, abc).unwrap();
        check_sha256(&f, &abc.to_uppercase()).unwrap();
        let e = check_sha256(&f, LINUX_SHA256).unwrap_err().to_string();
        assert!(e.contains("refusing"), "{e}");
    }

    #[test]
    fn has_files_wants_the_whole_per_os_set() {
        let d = scratch("gbe-has");
        assert!(!has_files(&d, Os::Linux) && !has_files(&d, Os::Windows));
        put(&d.join("linux64/libsteam_api.so"), b"x");
        assert!(!has_files(&d, Os::Linux));
        put(&d.join("linux64/steamclient.so"), b"x");
        assert!(has_files(&d, Os::Linux) && !has_files(&d, Os::Windows));
        put(&d.join("win64/steam_api64.dll"), b"x");
        assert!(has_files(&d, Os::Windows));
    }

    #[cfg(target_os = "linux")]
    fn tarball(d: &Path, steamclient: &[u8]) -> PathBuf {
        let src = d.join("src");
        put(&src.join("release/regular/x64/libsteam_api.so"), b"..configs.main.ini..");
        put(&src.join("release/regular/x64/steamclient.so"), steamclient);
        put(&src.join("release/regular/x86/libsteam_api.so"), b"x86");
        let a = d.join("a.tar.bz2");
        let st = Command::new("tar").arg("-cjf").arg(&a).arg("-C").arg(&src).arg("release").status().unwrap();
        assert!(st.success());
        a
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn extraction_installs_the_x64_pair_and_refuses_a_non_emulator() {
        let d = scratch("gbe-extract");
        let dest = d.join("gbe");
        let files = install_from_archive(&tarball(&d.join("good"), b"configs.main.ini"), &dest).unwrap();
        assert!(has_files(&dest, Os::Linux));
        assert_eq!(std::fs::read(dest.join("linux64/libsteam_api.so")).unwrap(), b"..configs.main.ini..");
        assert_eq!(files.as_object().unwrap().len(), 2);
        assert!(!dest.join(".fetch-gbe").exists());

        let bad = d.join("gbe-bad");
        let e = install_from_archive(&tarball(&d.join("bad"), b"Valve's steamclient"), &bad).unwrap_err().to_string();
        assert!(e.contains("not the gbe emulator"), "{e}");
        assert!(!bad.join("linux64").exists() && !bad.join(".fetch-gbe").exists());
    }

    #[test]
    fn a_given_archive_is_hash_checked_before_extraction() {
        let d = scratch("gbe-given");
        let a = d.join("emu.tar.bz2");
        put(&a, b"not the release");
        let dest = d.join("gbe");
        let o = FetchOpts { dest: &dest, cache: &d.join("cache"), archive: Some(&a) };
        let e = fetch_linux(&o, |_| {}).unwrap_err().to_string();
        assert!(e.contains("the pin is"), "{e}");
        assert!(!dest.exists());
    }
}
