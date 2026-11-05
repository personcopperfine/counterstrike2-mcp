//! Content-path rules for the `.vmdl` tools.
//!
//! A `path` argument is accepted in three forms and resolved against one layout
//! (`app_root` = the mounted game install, `content/` = its content root):
//!
//! 1. **host absolute** — `/home/…/workshop/run/app/content/csgo_addons/mcp_test/models/x.vmdl`;
//! 2. **Windows** (`Z:\app\content\…`, `z:/app/…`) through the shared
//!    [`PathMap`] — an unmapped drive is refused, never guessed;
//! 3. **content-relative** — `csgo_addons/mcp_test/models/x.vmdl` under `<app_root>/content/`;
//! 4. **addon-relative** — `models/x.vmdl` / `particles/x.vpcf` under the selected addon (as
//!    `compile_map` / `open_map` take `maps/...`). A relative path whose first segment is
//!    `csgo_addons`, `core` or `csgo` is content-relative (form 3); anything else is addon-relative.
//!
//! Both resolvers take the required extension (`"vmdl"` for the ModelDoc route, `"vpcf"` for the
//! particle route) and refuse any `..` segment. Reads must exist and
//! stay under `<app_root>/content/`; writes must stay under
//! `<app_root>/content/csgo_addons/<addon>/` (core content is read-only to the editor). Parent
//! directories of a write do not have to exist (the deepest existing ancestor is canonicalized, so
//! a symlink out of the addon is still caught) — `model_create` makes them.
//!
//! The derived forms the tools hand to the bridge/`resourcecompiler` are content-relative
//! (`content_rel`), container absolute (`container_path`, `/app/content/…`) and the compile output
//! (`compiled_output`, `<app_root>/game/…/<name>.vmdl_c`).

use crate::vmdl::VmdlError;
use cs2_mcp_common::pathmap::PathMap;
use std::path::{Path, PathBuf};

/// The layout a host path is resolved and checked against.
#[derive(Debug, Clone)]
pub struct ContentPaths {
    /// The mounted game install (`<project>/workshop/run/app`).
    pub app_root: PathBuf,
    /// The selected addon (`HammerState::addon`).
    pub addon: String,
}

impl ContentPaths {
    pub fn new(app_root: impl Into<PathBuf>, addon: impl Into<String>) -> ContentPaths {
        ContentPaths {
            app_root: app_root.into(),
            addon: addon.into(),
        }
    }

    /// `<app_root>/content`.
    pub fn content_root(&self) -> PathBuf {
        self.app_root.join("content")
    }

    /// `<app_root>/content/csgo_addons/<addon>`.
    pub fn addon_root(&self) -> PathBuf {
        self.content_root().join("csgo_addons").join(&self.addon)
    }

    /// Resolve a **read** of a `.vmdl`: the file must exist and be under `<app_root>/content/`.
    pub fn resolve_read(&self, input: &str, map: &PathMap) -> Result<PathBuf, VmdlError> {
        self.resolve_read_ext(input, map, "vmdl")
    }

    /// [`ContentPaths::resolve_read`] for another extension (`"vpcf"` for the particle file
    /// route): the file must exist, carry that extension and be under `<app_root>/content/`.
    pub fn resolve_read_ext(
        &self,
        input: &str,
        map: &PathMap,
        ext: &str,
    ) -> Result<PathBuf, VmdlError> {
        let candidate = self.candidate(input, map)?;
        require_ext(&candidate, ext)?;
        let real = std::fs::canonicalize(&candidate).map_err(|error| {
            VmdlError::Path(format!("{} cannot be read: {error}", candidate.display()))
        })?;
        let content = self.content_root();
        if !under(&real, &content) {
            return Err(VmdlError::Path(format!(
                "{} is outside the content root {}",
                real.display(),
                content.display()
            )));
        }
        Ok(real)
    }

    /// Resolve a **write** of a `.vmdl`: the result must be under
    /// `<app_root>/content/csgo_addons/<addon>/`. The file itself need not exist yet.
    pub fn resolve_write(&self, input: &str, map: &PathMap) -> Result<PathBuf, VmdlError> {
        self.resolve_write_ext(input, map, "vmdl")
    }

    /// [`ContentPaths::resolve_write`] for another extension (`"vpcf"` for the particle file
    /// route): the result must carry that extension and be under
    /// `<app_root>/content/csgo_addons/<addon>/`. The file itself need not exist yet.
    pub fn resolve_write_ext(
        &self,
        input: &str,
        map: &PathMap,
        ext: &str,
    ) -> Result<PathBuf, VmdlError> {
        let candidate = self.candidate(input, map)?;
        require_ext(&candidate, ext)?;
        let resolved = resolve_parent(&candidate)?;
        let addon_root = self.addon_root();
        if !under(&resolved, &addon_root) {
            let what = if ext.eq_ignore_ascii_case("vmdl") {
                "models"
            } else {
                "files"
            };
            return Err(VmdlError::Path(format!(
                "{} is outside content/csgo_addons/{}: only {what} of the active addon can be written; {}",
                resolved.display(),
                self.addon,
                self.accepted_forms(ext)
            )));
        }
        Ok(resolved)
    }

    /// Resolve a **directory** inside the active addon (no extension rule): the compile tools
    /// take a folder of sources. The result must be under
    /// `<app_root>/content/csgo_addons/<addon>/` (symlinks out of it are caught by the
    /// canonicalisation); it need not exist.
    pub fn resolve_write_dir(&self, input: &str, map: &PathMap) -> Result<PathBuf, VmdlError> {
        let candidate = self.candidate(input.trim_end_matches(['/', '\\']), map)?;
        let resolved = resolve_parent(&candidate)?;
        let addon_root = self.addon_root();
        if !under(&resolved, &addon_root) {
            return Err(VmdlError::Path(format!(
                "{} is outside content/csgo_addons/{}: only files of the active addon can be written; {}",
                resolved.display(),
                self.addon,
                self.accepted_forms("vpcf")
            )));
        }
        Ok(resolved)
    }

    /// `host` -> `csgo_addons/<addon>/models/x.vmdl` (forward slashes), relative to the content
    /// root; `None` when `host` is not inside it. For display and `cargo bridge runtime open` (vtmsend);
    /// **not** what `modeldoc.open` takes (see [`ContentPaths::addon_rel`]).
    pub fn content_rel(&self, host: &Path) -> Option<String> {
        let rel = strip_prefix(&self.content_root(), host)?;
        Some(rel.to_string_lossy().replace('\\', "/"))
    }

    /// `host` -> `models/x.vmdl` (forward slashes), relative to the addon content root; `None`
    /// when `host` is not inside the addon. This is the form the bridge's `modeldoc.open` takes:
    /// `OpenFile`'s `CONTENT`/`GAME` search paths are rooted at the addon (live 2026-10-12: the
    /// `csgo_addons/<addon>/…` form returns 0).
    pub fn addon_rel(&self, host: &Path) -> Option<String> {
        let rel = strip_prefix(&self.addon_root(), host)?;
        Some(rel.to_string_lossy().replace('\\', "/"))
    }

    /// `host` -> `/app/content/…` inside the tools container (what `resourcecompiler.exe -i`
    /// needs); `None` when `host` is not inside the content root.
    pub fn container_path(&self, host: &Path) -> Option<String> {
        self.content_rel(host)
            .map(|rel| format!("/app/content/{rel}"))
    }

    /// The compiled output of a source `.vmdl`:
    /// `<app_root>/game/csgo_addons/<addon>/…/x.vmdl_c`; `None` when `host_vmdl` is not inside the
    /// content root.
    pub fn compiled_output(&self, host_vmdl: &Path) -> Option<PathBuf> {
        self.content_rel(host_vmdl)
            .map(|rel| self.app_root.join("game").join(format!("{rel}_c")))
    }

    /// The addon `host` belongs to: `<app_root>/content/csgo_addons/<name>/…` or
    /// `<app_root>/game/csgo_addons/<name>/…` -> `<name>`; `None` outside `csgo_addons`.
    pub fn addon_of(&self, host: &Path) -> Option<String> {
        let roots = [
            self.content_root().join("csgo_addons"),
            self.app_root.join("game").join("csgo_addons"),
        ];
        roots.iter().find_map(|root| {
            let rel = strip_prefix(root, host)?;
            let text = rel.to_string_lossy().replace('\\', "/");
            let name = text.split('/').next().filter(|n| !n.is_empty())?;
            Some(name.to_string())
        })
    }

    /// Why `host` is refused when it sits in ANOTHER addon than the selected one: names that addon,
    /// how the selected one was chosen (`source`) and how to work in the other. `None` when `host`
    /// is in our addon or in none.
    pub fn other_addon_hint(&self, host: &Path, source: crate::state::AddonSource) -> Option<String> {
        use crate::state::AddonSource;
        let other = self.addon_of(host)?;
        if other.eq_ignore_ascii_case(&self.addon) {
            return None;
        }
        let (how, undo) = match source {
            AddonSource::SetAddon => (
                "pinned by set_addon",
                " or set_addon {addon:null} to follow the document",
            ),
            AddonSource::Document => ("the active document's addon", ""),
            AddonSource::Flag => ("from --addon; no document is open", ""),
        };
        Some(format!(
            "it is in addon {other:?}, but the selected addon is {:?} ({how}): \
             call set_addon {{addon:\"{other}\"}}{undo}",
            self.addon
        ))
    }

    /// True when `host` is inside `<app_root>/content/csgo_addons/<addon>/`.
    pub fn in_addon(&self, host: &Path) -> bool {
        under(host, &self.addon_root())
    }

    /// The three accepted input forms -> a host path (never checked for existence here).
    fn candidate(&self, input: &str, map: &PathMap) -> Result<PathBuf, VmdlError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(VmdlError::Path("path is empty".to_string()));
        }
        if input.split(['/', '\\']).any(|segment| segment == "..") {
            return Err(VmdlError::Path(format!("path contains `..`: {input:?}")));
        }
        if is_windows_path(input) {
            return map.windows_to_host(input).ok_or_else(|| {
                VmdlError::Path(format!(
                    "windows path {input:?} has no host mapping (check --path-map)"
                ))
            });
        }
        let host = Path::new(input);
        if host.is_absolute() {
            return Ok(host.to_path_buf());
        }
        let mut rel = input.replace('\\', "/");
        while let Some(rest) = rel.strip_prefix("./") {
            rel = rest.to_string();
        }
        let first = rel.split('/').next().unwrap_or_default().to_string();
        if ["csgo_addons", "core", "csgo"]
            .iter()
            .any(|root| first.eq_ignore_ascii_case(root))
        {
            // the content roots are lower case on disk: normalise the spelling so `CSGO_ADDONS/..`
            // is content-relative (and checked) instead of becoming a folder inside the addon
            let rel = format!("{}{}", first.to_ascii_lowercase(), &rel[first.len()..]);
            Ok(self.content_root().join(rel))
        } else {
            Ok(self.addon_root().join(rel))
        }
    }

    /// The accepted write forms, with an example, appended to the "outside the addon" errors.
    fn accepted_forms(&self, ext: &str) -> String {
        let dir = if ext.eq_ignore_ascii_case("vmdl") { "models" } else { "particles" };
        format!(
            "accepted forms: addon-relative `{dir}/x.{ext}`, content path \
             `csgo_addons/{addon}/{dir}/x.{ext}`, or an absolute host path inside \
             content/csgo_addons/{addon}/",
            addon = self.addon
        )
    }
}

/// `C:\…`, `C:/…`, `\\server\share`, `//?/C:/…`.
fn is_windows_path(input: &str) -> bool {
    let bytes = input.as_bytes();
    input.starts_with("\\\\")
        || input.starts_with("//?/")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

fn require_ext(path: &Path, ext: &str) -> Result<(), VmdlError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case(ext) {
        return Err(VmdlError::Path(format!(
            "{} is not a .{ext} file",
            path.display()
        )));
    }
    Ok(())
}

/// Canonicalize the deepest existing ancestor of `path` and join the missing tail back on, so a
/// target below a directory that does not exist yet (a fresh `models/<new>/` folder) still
/// resolves through its real parent.
fn resolve_parent(path: &Path) -> Result<PathBuf, VmdlError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(VmdlError::Io)?.join(path)
    };
    let name = path
        .file_name()
        .ok_or_else(|| VmdlError::Path(format!("{} has no file name", path.display())))?
        .to_os_string();
    let mut tail = vec![name];
    let mut dir = path.parent().map(Path::to_path_buf);
    while let Some(current) = dir {
        if let Ok(real) = std::fs::canonicalize(&current) {
            let mut resolved = real;
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return Ok(resolved);
        }
        match (current.file_name(), current.parent()) {
            (Some(part), Some(parent)) => {
                tail.push(part.to_os_string());
                dir = Some(parent.to_path_buf());
            }
            _ => break,
        }
    }
    Err(VmdlError::Path(format!(
        "cannot resolve the parent directory of {}",
        path.display()
    )))
}

/// Is `path` inside `root`? Canonical first (so a symlink cannot escape), a case-insensitive
/// lexical comparison as the fallback for paths that do not exist yet.
fn under(path: &Path, root: &Path) -> bool {
    if let (Ok(path), Ok(root)) = (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        return path.starts_with(root);
    }
    strip_prefix(root, path).is_some()
}

/// `root` canonicalized when it exists (a project root reached through a symlink), else as given.
/// `resolve_parent` returns a canonical path for a file that does not exist yet, so the lexical
/// fallbacks below must compare it against the canonical root, not the symlinked spelling.
fn real_or_given(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// `path` relative to `root`, or `None`. Case-insensitive when the two cannot be canonicalized.
fn strip_prefix(root: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(rel) = path.strip_prefix(root) {
        return Some(rel.to_path_buf());
    }
    if let (Ok(real_root), Ok(real_path)) = (std::fs::canonicalize(root), std::fs::canonicalize(path))
    {
        if let Ok(rel) = real_path.strip_prefix(&real_root) {
            return Some(rel.to_path_buf());
        }
    }
    let real_root = real_or_given(root);
    if let Ok(rel) = path.strip_prefix(&real_root) {
        return Some(rel.to_path_buf());
    }
    let root_text = root.to_string_lossy().replace('\\', "/");
    let path_text = path.to_string_lossy().replace('\\', "/");
    let root_text = root_text.trim_end_matches('/');
    if path_text.len() < root_text.len() || !path_text.is_char_boundary(root_text.len()) {
        return None;
    }
    let (head, tail) = path_text.split_at(root_text.len());
    if !head.eq_ignore_ascii_case(root_text) {
        return None;
    }
    match tail.strip_prefix('/') {
        Some(rest) => Some(PathBuf::from(rest)),
        None if tail.is_empty() => Some(PathBuf::new()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AddonSource;

    fn paths() -> ContentPaths {
        ContentPaths::new("/proj/workshop/run/app", "mcp_test")
    }

    #[test]
    fn addon_of_names_the_csgo_addons_directory() {
        let p = paths();
        let other = Path::new("/proj/workshop/run/app/content/csgo_addons/ti_convoy/maps/ti_convoy.vmap");
        assert_eq!(p.addon_of(other).as_deref(), Some("ti_convoy"));
        let ours = Path::new("/proj/workshop/run/app/content/csgo_addons/mcp_test/maps/probe.vmap");
        assert_eq!(p.addon_of(ours).as_deref(), Some("mcp_test"));
        let game = Path::new("/proj/workshop/run/app/game/csgo_addons/ti_convoy/maps/ti_convoy.vpk");
        assert_eq!(p.addon_of(game).as_deref(), Some("ti_convoy"));
        // outside csgo_addons, or the bare csgo_addons directory: no addon
        assert_eq!(p.addon_of(Path::new("/proj/workshop/run/app/content/csgo/maps/x.vmap")), None);
        assert_eq!(p.addon_of(Path::new("/proj/workshop/run/app/content/csgo_addons")), None);
        assert_eq!(p.addon_of(Path::new("/elsewhere/csgo_addons/ti_convoy/maps/x.vmap")), None);
    }

    #[test]
    fn other_addon_hint_only_for_another_addon() {
        let p = paths();
        let other = Path::new("/proj/workshop/run/app/content/csgo_addons/ti_convoy/maps/ti_convoy.vmap");
        let hint = p.other_addon_hint(other, AddonSource::SetAddon).expect("another addon is hinted");
        assert!(hint.contains("\"ti_convoy\"") && hint.contains("set_addon"), "{hint}");
        assert!(hint.contains("\"mcp_test\"") && hint.contains("pinned by set_addon"), "{hint}");
        let doc_hint = p.other_addon_hint(other, AddonSource::Document).expect("hinted");
        assert!(doc_hint.contains("active document") && !doc_hint.contains("pinned"), "{doc_hint}");
        let flag_hint = p.other_addon_hint(other, AddonSource::Flag).expect("hinted");
        assert!(flag_hint.contains("--addon") && !flag_hint.contains("pinned"), "{flag_hint}");
        let ours = Path::new("/proj/workshop/run/app/content/csgo_addons/MCP_TEST/maps/probe.vmap");
        assert_eq!(p.other_addon_hint(ours, AddonSource::Flag), None, "same addon (case-insensitive) is no mismatch");
        assert_eq!(p.other_addon_hint(Path::new("/proj/workshop/run/app/content/csgo/maps/x.vmap"), AddonSource::Flag), None);
    }

    #[test]
    fn addon_relative_and_content_relative_resolve_to_the_same_file() {
        let p = paths();
        let map = PathMap::default();
        let want = PathBuf::from("/proj/workshop/run/app/content/csgo_addons/mcp_test/particles/a.vpcf");
        assert_eq!(p.candidate("particles/a.vpcf", &map).unwrap(), want);
        assert_eq!(p.candidate("csgo_addons/mcp_test/particles/a.vpcf", &map).unwrap(), want);
        assert_eq!(p.candidate("particles\\a.vpcf", &map).unwrap(), want);
        // core / csgo stay content-relative (reads of shipped content)
        assert_eq!(
            p.candidate("core/particles/x.vpcf", &map).unwrap(),
            PathBuf::from("/proj/workshop/run/app/content/core/particles/x.vpcf")
        );
    }

    #[test]
    fn root_spelling_is_case_insensitive_and_dot_slash_is_stripped() {
        let p = paths();
        let map = PathMap::default();
        let want = PathBuf::from("/proj/workshop/run/app/content/csgo_addons/mcp_test/particles/a.vpcf");
        assert_eq!(p.candidate("CSGO_ADDONS/mcp_test/particles/a.vpcf", &map).unwrap(), want);
        assert_eq!(p.candidate("./particles/a.vpcf", &map).unwrap(), PathBuf::from("/proj/workshop/run/app/content/csgo_addons/mcp_test/particles/a.vpcf"));
        assert_eq!(
            p.candidate("Core/particles/x.vpcf", &map).unwrap(),
            PathBuf::from("/proj/workshop/run/app/content/core/particles/x.vpcf")
        );
        // a wrong addon in the upper-case spelling is refused like the lower-case one
        assert!(p.resolve_write_ext("CSGO_ADDONS/other/particles/a.vpcf", &map, "vpcf").is_err());
    }

    #[test]
    fn outside_write_error_names_the_accepted_forms() {
        let p = paths();
        let map = PathMap::default();
        let err = p.resolve_write_ext("csgo_addons/other/particles/a.vpcf", &map, "vpcf").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("addon-relative `particles/x.vpcf`") && text.contains("csgo_addons/mcp_test/particles/x.vpcf"), "{text}");
        assert!(text.contains("of the active addon can be written"), "{text}");
        let ok = p.resolve_write_ext("particles/new/a.vpcf", &map, "vpcf");
        assert!(ok.is_err() || ok.unwrap().ends_with("mcp_test/particles/new/a.vpcf"));
    }
}
