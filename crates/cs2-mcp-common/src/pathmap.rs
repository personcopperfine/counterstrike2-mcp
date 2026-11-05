//! Windows -> host path mapping (port of `bridge_client.windows_to_host`), shared by both servers.
//!
//! Longest-prefix map: `Z:\app` -> `<project>/workshop/run/app`, `Z:\cache` -> `.../run/cache`,
//! `C:` -> `<pfx>/drive_c`; then case-insensitive resolution (Linux is case-sensitive, the game's
//! casing is not guaranteed); a drive-less relative path is tried under the content root
//! (`<app>/content/`). Anything else is `None` (reported as `PathNotMapped`, never guessed), and
//! any `..` segment is refused outright.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct PathMap {
    /// (lower-case windows prefix without trailing `\`, host root)
    pub entries: Vec<(String, PathBuf)>,
    /// `<app>/content` for drive-less relative paths
    pub content_root: Option<PathBuf>,
}

impl PathMap {
    /// The container layout: `<project>/workshop/run/app` is mounted at `/app`, Wine's `Z:` is the
    /// container root, `C:` is the prefix.
    pub fn defaults(project_root: &Path, pfx: &Path) -> PathMap {
        let app = project_root.join("workshop/run/app");
        PathMap {
            entries: vec![
                ("z:\\app".into(), app.clone()),
                ("z:\\cache".into(), project_root.join("workshop/run/cache")),
                ("c:".into(), pfx.join("drive_c")),
            ],
            content_root: Some(app.join("content")),
        }
    }

    /// The map a server starts with. Linux host: [`PathMap::defaults`] with `z:\app` (and the
    /// content root) at `app_root` (`--app-root`). Windows host (the game runs natively next to
    /// the server): no prefix entries, a Windows path the bridge reports is already a host path
    /// (see [`PathMap::windows_to_host`]); drive-less relative paths resolve under
    /// `<app_root>/content`.
    pub fn for_host(project_root: &Path, app_root: &Path, pfx: &Path) -> PathMap {
        if cfg!(windows) {
            return PathMap {
                entries: Vec::new(),
                content_root: Some(app_root.join("content")),
            };
        }
        let mut map = PathMap::defaults(project_root, pfx);
        map.extend(&PathMap {
            entries: vec![("z:\\app".into(), app_root.to_path_buf())],
            content_root: Some(app_root.join("content")),
        });
        map
    }

    /// `CS2_MCP_PATH_MAP` / `--path-map` syntax: `z:\app=/x;c:=/y` (`,` also separates).
    pub fn parse(spec: &str) -> anyhow::Result<PathMap> {
        let mut entries = Vec::new();
        for chunk in spec
            .replace(',', ";")
            .split(';')
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            let (win, host) = chunk
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("path map entry without '=': {chunk:?}"))?;
            entries.push((normalize_key(win), PathBuf::from(host.trim())));
        }
        Ok(PathMap {
            entries,
            content_root: None,
        })
    }

    /// Later entries override earlier ones with the same key.
    pub fn extend(&mut self, other: &PathMap) {
        for (k, v) in &other.entries {
            self.entries.retain(|(ek, _)| ek != k);
            self.entries.push((k.clone(), v.clone()));
        }
        if other.content_root.is_some() {
            self.content_root = other.content_root.clone();
        }
    }

    pub fn windows_to_host(&self, path: &str) -> Option<PathBuf> {
        let mut p = path.trim().replace('/', "\\");
        if p.is_empty() || p.split('\\').any(|seg| seg == "..") {
            return None;
        }
        if let Some(rest) = p.strip_prefix("\\\\?\\UNC\\") {
            p = format!("\\\\{rest}");
        } else if let Some(rest) = p.strip_prefix("\\\\?\\") {
            p = rest.to_string();
        }
        let key = p.to_ascii_lowercase();
        let mut best: Option<(&String, &PathBuf)> = None;
        for (win, host) in &self.entries {
            if (key == *win || key.starts_with(&format!("{win}\\")))
                && best.is_none_or(|(w, _)| win.len() > w.len())
            {
                best = Some((win, host));
            }
        }
        if let Some((win, host)) = best {
            let suffix = p[win.len()..].trim_start_matches('\\');
            let candidate = if suffix.is_empty() {
                host.clone()
            } else {
                host.join(suffix.replace('\\', "/"))
            };
            return Some(case_insensitive_resolve(&candidate).unwrap_or(candidate));
        }
        let first = p.split('\\').next().unwrap_or("");
        if !first.contains(':') && !p.starts_with('\\') {
            let content = self.content_root.as_ref()?;
            return case_insensitive_resolve(&content.join(p.replace('\\', "/")));
        }
        // Windows host: an absolute Windows path (`C:\...`, `\\server\share\...`) the bridge
        // reports is a path on this machine (the filesystem is case-insensitive already)
        if cfg!(windows) && ((first.len() == 2 && first.ends_with(':')) || p.starts_with("\\\\")) {
            return Some(PathBuf::from(p));
        }
        None
    }
}

fn normalize_key(win: &str) -> String {
    win.trim()
        .to_ascii_lowercase()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string()
}

/// `path` if it exists, else a case-insensitive match, else `None`.
pub fn case_insensitive_resolve(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return Some(path.to_path_buf());
    }
    let mut cur = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        PathBuf::from(".")
    };
    for part in path.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str(),
        _ => None,
    }) {
        let entries = std::fs::read_dir(&cur).ok()?;
        let hit = entries
            .flatten()
            .map(|e| e.file_name())
            .find(|n| n.to_string_lossy().eq_ignore_ascii_case(part))?;
        cur = cur.join(hit);
    }
    cur.exists().then_some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--app-root` moves `z:\app` and the content root; the other container entries stay.
    #[cfg(not(windows))]
    #[test]
    fn for_host_points_z_app_at_the_app_root() {
        let m = PathMap::for_host(Path::new("/proj"), Path::new("/games/cs2"), Path::new("/pfx"));
        assert_eq!(
            m.windows_to_host("Z:\\app\\renders\\a.png"),
            Some(PathBuf::from("/games/cs2/renders/a.png"))
        );
        assert_eq!(m.content_root, Some(PathBuf::from("/games/cs2/content")));
        assert_eq!(
            m.windows_to_host("z:\\cache\\x"),
            Some(PathBuf::from("/proj/workshop/run/cache/x"))
        );
        // a native Windows path is not a host path on Linux
        assert_eq!(m.windows_to_host("D:\\Users\\t\\x.png"), None);
    }

    fn pm() -> PathMap {
        PathMap::defaults(
            Path::new("/proj"),
            Path::new("/proj/workshop/run/app/compatdata/730/pfx"),
        )
    }

    #[test]
    fn dot_dot_segments_are_refused() {
        let m = pm();
        assert!(m.windows_to_host("Z:\\app\\..\\secret.txt").is_none());
        assert!(m
            .windows_to_host("Z:\\app\\content\\..\\..\\etc\\passwd")
            .is_none());
        assert!(m.windows_to_host("Z:/app/content/../../x").is_none());
        assert!(m.windows_to_host("maps\\..\\probe.vmap").is_none());
        // a name that merely contains dots is fine
        assert!(m.windows_to_host("Z:\\app\\content\\a..b.vmap").is_some());
    }

    #[test]
    fn z_app_maps_to_the_workshop_tree() {
        let host = pm()
            .windows_to_host("Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap")
            .unwrap();
        assert_eq!(
            host,
            PathBuf::from("/proj/workshop/run/app/content/csgo_addons/mcp_test/maps/probe.vmap")
        );
    }

    #[test]
    fn c_drive_maps_to_the_prefix() {
        let host = pm().windows_to_host("C:\\users\\steamuser\\x.txt").unwrap();
        assert!(host.ends_with("drive_c/users/steamuser/x.txt"), "{host:?}");
    }

    #[test]
    fn custom_map_and_unknown_drive() {
        let mut m = pm();
        m.extend(&PathMap::parse("x:\\mvp=/tmp/opencode/mvp").unwrap());
        assert_eq!(
            m.windows_to_host("X:\\mvp\\probe.vmap").unwrap(),
            PathBuf::from("/tmp/opencode/mvp/probe.vmap")
        );
        assert!(m.windows_to_host("Q:\\nothing\\x.vmap").is_none());
    }

    #[test]
    fn forward_slashes_and_long_prefix() {
        let a = pm().windows_to_host("//?/Z:/app/content/a.vmap").unwrap();
        let b = pm().windows_to_host("Z:/app/content/a.vmap").unwrap();
        assert_eq!(a, b);
        assert!(a.ends_with("a.vmap"));
    }

    #[test]
    fn parse_path_map() {
        let m = PathMap::parse("x:\\mvp=/tmp/opencode/mvp;y:=/tmp/opencode/y").unwrap();
        assert_eq!(
            m.entries[0],
            ("x:\\mvp".to_string(), PathBuf::from("/tmp/opencode/mvp"))
        );
        assert_eq!(
            m.entries[1],
            ("y:".to_string(), PathBuf::from("/tmp/opencode/y"))
        );
        assert!(PathMap::parse("nonsense").is_err());
    }

    #[test]
    fn case_insensitive_resolution_and_content_root() {
        let dir = std::env::temp_dir().join(format!("cs2mcp-paths-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Content/Maps")).unwrap();
        std::fs::write(dir.join("Content/Maps/Probe.vmap"), b"x").unwrap();
        let mut m = PathMap::parse(&format!("z:\\app={}", dir.display())).unwrap();
        m.content_root = Some(dir.join("content"));
        assert_eq!(
            m.windows_to_host("z:\\app\\content\\maps\\probe.vmap")
                .unwrap(),
            dir.join("Content/Maps/Probe.vmap")
        );
        assert_eq!(
            m.windows_to_host("maps\\probe.vmap").unwrap(),
            dir.join("Content/Maps/Probe.vmap")
        );
        assert!(m.windows_to_host("maps\\missing.vmap").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
