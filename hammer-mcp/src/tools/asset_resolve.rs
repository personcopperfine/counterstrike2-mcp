//! Shared asset resolution for `report_ready` (materials check) and `find_assets`: where a
//! compiled resource can come from (the addon's game dir, the game's loose dirs, a map vpk, the
//! two game paks), the cached VPK opener, the case-insensitive disk lookup, and the one place
//! that decides whether a material's textures resolve ([`Sources::check_textures`]).
use crate::state::HammerState;
use crate::vpk::VpkDir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Everything the disk checks need to know where things are.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) app_root: PathBuf,
    /// `<app>/content/csgo_addons/<addon>`
    pub(crate) content: PathBuf,
    /// `<app>/game/csgo_addons/<addon>`
    pub(crate) game: PathBuf,
}

impl Paths {
    pub(crate) fn of(state: &HammerState) -> Paths {
        let app_root = state.app_root();
        let addon = state.addon();
        Paths {
            content: app_root.join("content/csgo_addons").join(&addon),
            game: app_root.join("game/csgo_addons").join(&addon),
            app_root,
        }
    }
}

type VpkCache = Mutex<HashMap<PathBuf, (Option<SystemTime>, u64, Arc<VpkDir>)>>;

/// A parsed `_dir.vpk`, cached per `(path, mtime, len)`: the csgo tree is 137k entries.
pub(crate) fn open_vpk(path: &Path) -> Result<Arc<VpkDir>, String> {
    static CACHE: OnceLock<VpkCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let key = (meta.modified().ok(), meta.len());
    if let Some((mtime, len, dir)) = cache.lock().unwrap_or_else(|p| p.into_inner()).get(path) {
        if (*mtime, *len) == key {
            return Ok(dir.clone());
        }
    }
    let dir = Arc::new(VpkDir::open(path).map_err(|e| e.to_string())?);
    cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(path.to_path_buf(), (key.0, key.1, dir.clone()));
    Ok(dir)
}

/// Normalised asset reference: lowercase, `/`, no leading slash.
pub(crate) fn norm(path: &str) -> String {
    path.trim().replace('\\', "/").trim_start_matches('/').to_ascii_lowercase()
}

/// `rel` under `root`, matching the given case first and then each path component
/// case-insensitively (the Linux filesystem is case-sensitive, Source 2 paths are not).
pub(crate) fn find_ci(root: &Path, rel: &str) -> Option<PathBuf> {
    let direct = root.join(rel);
    if direct.is_file() {
        return Some(direct);
    }
    let mut cur = root.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        let exact = cur.join(part);
        if exact.exists() {
            cur = exact;
            continue;
        }
        let hit = std::fs::read_dir(&cur)
            .ok()?
            .flatten()
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part))?;
        cur = hit.path();
    }
    cur.is_file().then_some(cur)
}

/// Where a compiled resource can come from: the addon's game dir, the game's loose dirs, the map
/// vpk and the two game paks.
pub(crate) struct Sources {
    pub(crate) dirs: Vec<PathBuf>,
    pub(crate) paks: Vec<Arc<VpkDir>>,
    /// The game paks that could not be opened, with why.
    pub(crate) pak_errors: Vec<String>,
}

impl Sources {
    pub(crate) fn new(paths: &Paths, map_vpk: Option<&Path>) -> Sources {
        let mut paks = Vec::new();
        let mut pak_errors = Vec::new();
        if let Some(v) = map_vpk.and_then(|p| open_vpk(p).ok()) {
            paks.push(v);
        }
        for pak in ["game/csgo/pak01_dir.vpk", "game/core/pak01_dir.vpk"] {
            match open_vpk(&paths.app_root.join(pak)) {
                Ok(v) => paks.push(v),
                Err(e) => pak_errors.push(format!("{pak}: {e}")),
            }
        }
        Sources {
            dirs: vec![paths.game.clone(), paths.app_root.join("game/csgo"), paths.app_root.join("game/core")],
            paks,
            pak_errors,
        }
    }

    pub(crate) fn has(&self, compiled: &str) -> bool {
        self.paks.iter().any(|v| v.contains(compiled)) || self.dirs.iter().any(|d| find_ci(d, compiled).is_some())
    }

    pub(crate) fn read(&self, compiled: &str) -> Option<Vec<u8>> {
        if let Some(bytes) =
            self.dirs.iter().find_map(|d| find_ci(d, compiled)).and_then(|p| std::fs::read(p).ok())
        {
            return Some(bytes);
        }
        self.paks.iter().find(|v| v.contains(compiled)).and_then(|v| v.read(compiled).ok())
    }
}


impl Sources {
    /// Do the `.vtex` references of the compiled material `key` (lowercase `materials/x.vmat`)
    /// resolve? `Ok((textures checked, missing))`, or `Err(reason)` when nothing can be said: the
    /// compiled file cannot be read or parsed, has no reference list, or its list is truncated
    /// or malformed (never read as "no textures"). `seen` memoises per texture across calls.
    pub(crate) fn check_textures(
        &self,
        key: &str,
        seen: &mut HashMap<String, bool>,
    ) -> Result<(usize, Vec<String>), String> {
        let bytes = self.read(&format!("{key}_c")).ok_or("the compiled file could not be read")?;
        let resource =
            crate::resource::Resource::parse(&bytes).map_err(|e| format!("the compiled file is not a valid resource: {e}"))?;
        let list = resource.external_refs_checked()?;
        let mut checked = 0usize;
        let mut missing = Vec::new();
        for tex in list.iter().filter(|t| t.to_ascii_lowercase().ends_with(".vtex")) {
            checked += 1;
            let tex_c = format!("{}_c", tex.replace('\\', "/"));
            let found = *seen
                .entry(tex.to_ascii_lowercase())
                .or_insert_with(|| self.has(&tex_c));
            if !found {
                missing.push(tex.clone());
            }
        }
        Ok((checked, missing))
    }
}
