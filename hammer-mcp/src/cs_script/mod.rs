//! The cs_script project of an addon: a TypeScript project (rollup + tsc, the Source2ZE
//! `create-cs-script` boilerplate) kept at `content/csgo_addons/<addon>/cs_script/`, one rollup
//! target per script, building to `content/csgo_addons/<addon>/maps/scripts/<name>.js`.
//!
//! This module is the pure half (file layout, the rollup targets block, source-file guards); the
//! `cs_script` tool (`tools/cs_script.rs`) runs npm/tsc/rollup/node around it. Submodules: [`diag`]
//! (tsc/rollup output -> diagnostics) and [`types`] (the game's `point_script.d.ts` against the npm
//! `@s2ze/types`).
//!
//! The template files in `hammer-mcp/templates/cs_script/` are vendored from `create-cs-script`
//! [`UPSTREAM_VERSION`] (https://github.com/Source2ZE/cs_script, MIT, commit 1efee7c); the changes
//! against upstream are: the targets array sits between `cs-script:targets` markers (so `add` can
//! edit it), `CS_SCRIPT_ONLY` selects one target, `noEmitOnError`, a generated-file banner, and the
//! README. The licence text is written into every project as `LICENSE.create-cs-script`.
pub mod diag;
pub mod run;
pub mod types;

use std::path::{Path, PathBuf};

/// The `create-cs-script` release the templates come from.
pub const UPSTREAM_VERSION: &str = "2.23.1";
/// The `@s2ze/*` packages the boilerplate depends on (create-cs-script's `s2zePackages`, sorted).
pub const S2ZE_PACKAGES: [&str; 6] = [
    "@s2ze/debug",
    "@s2ze/math",
    "@s2ze/prototype-extend",
    "@s2ze/scheduler",
    "@s2ze/tsconfig",
    "@s2ze/types",
];

pub const TPL_PACKAGE_JSON: &str = include_str!("../../templates/cs_script/package.json");
pub const TPL_ROLLUP: &str = include_str!("../../templates/cs_script/rollup.config.js");
pub const TPL_GITIGNORE: &str = include_str!("../../templates/cs_script/gitignore");
pub const TPL_README: &str = include_str!("../../templates/cs_script/README.md");
pub const TPL_INDEX_TS: &str = include_str!("../../templates/cs_script/index.ts");
pub const TPL_TSCONFIG: &str = include_str!("../../templates/cs_script/tsconfig.json");
pub const TPL_LICENSE: &str = include_str!("../../templates/cs_script/LICENSE.create-cs-script");
/// The script comparing two `cs_script/point_script` declarations (run by node, see [`types`]).
pub const FINGERPRINT_JS: &str = include_str!("../../templates/cs_script/api-fingerprint.mjs");

pub const BEGIN_MARKER: &str = "// cs-script:targets:begin";
pub const END_MARKER: &str = "// cs-script:targets:end";
/// Longest script name (it becomes a file name and a keyvalue).
pub const NAME_MAX: usize = 48;
/// Largest source file `write` accepts.
pub const SOURCE_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CsError {
    #[error("{0}")]
    BadName(String),
    #[error("{0}")]
    BadFile(String),
    #[error("a cs_script project already exists at {0}: use update, add or build")]
    Exists(String),
    #[error("no cs_script project at {0}: run cs_script {{action:\"init\"}} first")]
    NoProject(String),
    #[error("script {0:?} already exists")]
    Duplicate(String),
    #[error("no script {name:?} in the project (scripts: {have:?}); add it first")]
    UnknownScript { name: String, have: Vec<String> },
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Io(String),
}

/// `[a-z0-9_]+`, at most [`NAME_MAX`] characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub fn check_name(name: &str) -> Result<(), CsError> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(CsError::BadName(format!(
            "script name {name:?} must match [a-z0-9_]+ (at most {NAME_MAX} characters)"
        )))
    }
}

/// Where the project of one addon lives.
#[derive(Debug, Clone)]
pub struct Project {
    pub addon: String,
    /// `content/csgo_addons/<addon>`.
    pub addon_root: PathBuf,
    /// `content/csgo_addons/<addon>/cs_script`.
    pub dir: PathBuf,
    /// `content/csgo_addons/<addon>/maps/scripts`.
    pub out_dir: PathBuf,
}

impl Project {
    pub fn new(addon: &str, addon_root: &Path) -> Project {
        Project {
            addon: addon.to_string(),
            addon_root: addon_root.to_path_buf(),
            dir: addon_root.join("cs_script"),
            out_dir: addon_root.join("maps").join("scripts"),
        }
    }

    pub fn exists(&self) -> bool {
        self.dir.join("package.json").is_file() && self.dir.join("rollup.config.js").is_file()
    }

    pub fn rollup_path(&self) -> PathBuf {
        self.dir.join("rollup.config.js")
    }

    pub fn src_dir(&self, name: &str) -> PathBuf {
        self.dir.join("src").join(name)
    }

    /// The built script: `maps/scripts/<name>.js`.
    pub fn js_path(&self, name: &str) -> PathBuf {
        self.out_dir.join(format!("{name}.js"))
    }

    /// The addon-relative form a point_script's keyvalue and `compile_asset` use.
    pub fn js_rel(name: &str) -> String {
        format!("maps/scripts/{name}.js")
    }

    pub fn vjs_keyvalue(name: &str) -> String {
        format!("maps/scripts/{name}.vjs")
    }

    pub fn require(&self) -> Result<(), CsError> {
        if self.exists() {
            Ok(())
        } else {
            Err(CsError::NoProject(self.dir.display().to_string()))
        }
    }

    /// The script names in `rollup.config.js`, in file order.
    pub fn targets(&self) -> Result<Vec<String>, CsError> {
        let text = std::fs::read_to_string(self.rollup_path())
            .map_err(|e| CsError::Io(format!("cannot read {}: {e}", self.rollup_path().display())))?;
        parse_targets(&text)
    }

    /// `name` must be one of [`Project::targets`].
    pub fn require_script(&self, name: &str) -> Result<(), CsError> {
        check_name(name)?;
        let have = self.targets()?;
        if have.iter().any(|n| n == name) {
            Ok(())
        } else {
            Err(CsError::UnknownScript { name: name.to_string(), have })
        }
    }

    /// `cs_script/src/<name>/<file>` for a source file `write`/`read` may touch: relative, each
    /// segment `[A-Za-z0-9_.-]` (never `.`/`..`), a `.ts` file, the script's own folder.
    pub fn source_path(&self, name: &str, file: &str) -> Result<PathBuf, CsError> {
        check_name(name)?;
        let bad = |why: &str| CsError::BadFile(format!("file {file:?} {why}: a relative path to a .ts file under src/{name}/ (letters, digits, _ . - only)"));
        if file.is_empty() || file.len() > 120 || file.starts_with('/') || file.contains('\\') {
            return Err(bad("is not allowed"));
        }
        let mut path = self.src_dir(name);
        for segment in file.split('/') {
            let clean = !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
            if !clean {
                return Err(bad("has a segment that is not allowed"));
            }
            path.push(segment);
        }
        if !file.ends_with(".ts") {
            return Err(bad("is not a .ts file"));
        }
        Ok(path)
    }
}

/// The npm package name of a project: `cs-script-<addon>` limited to npm's alphabet.
pub fn package_name(addon: &str) -> String {
    let cleaned: String = addon
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '~') { c } else { '-' })
        .collect();
    format!("cs-script-{}", cleaned.trim_matches(|c| c == '-' || c == '.'))
}

/// One `targets` entry (rollup.config.js).
pub fn target_line(name: &str) -> String {
    format!(
        "  {{ name: '{name}', input: 'src/{name}/index.ts', tsconfig: 'src/{name}/tsconfig.json', output: `${{OUTPUT_DIR}}/{name}.js` }},"
    )
}

/// The script names between the markers of a rollup.config.js.
pub fn parse_targets(rollup: &str) -> Result<Vec<String>, CsError> {
    let (begin, end) = marker_span(rollup)?;
    let re = regex::Regex::new(r"name:\s*'([a-z0-9_]+)'").expect("static regex");
    Ok(re.captures_iter(&rollup[begin..end]).map(|c| c[1].to_string()).collect())
}

fn marker_span(rollup: &str) -> Result<(usize, usize), CsError> {
    let shape = || {
        CsError::Config(format!(
            "rollup.config.js lost its `{BEGIN_MARKER}` / `{END_MARKER}` markers (they delimit the targets array the cs_script tool edits): restore them or edit the file by hand"
        ))
    };
    let begin = rollup.find(BEGIN_MARKER).ok_or_else(shape)? + BEGIN_MARKER.len();
    let end = rollup.find(END_MARKER).ok_or_else(shape)?;
    if end < begin {
        return Err(shape());
    }
    Ok((begin, end))
}

/// `rollup` with one more target appended to the targets array.
pub fn insert_target(rollup: &str, name: &str) -> Result<String, CsError> {
    check_name(name)?;
    if parse_targets(rollup)?.iter().any(|n| n == name) {
        return Err(CsError::Duplicate(name.to_string()));
    }
    let (begin, end) = marker_span(rollup)?;
    let region = &rollup[begin..end];
    // the closing `];` of the array is the last line of the region that is just that
    let close = region
        .lines()
        .rev()
        .find(|line| line.trim() == "];")
        .ok_or_else(|| CsError::Config("rollup.config.js: no `];` closing the targets array between the markers".into()))?;
    let offset = begin + region.rfind(close).expect("line of the region");
    let mut out = String::with_capacity(rollup.len() + 160);
    out.push_str(&rollup[..offset]);
    out.push_str(&target_line(name));
    out.push('\n');
    out.push_str(&rollup[offset..]);
    Ok(out)
}

/// `path` must stay inside `addon_root` once symlinks are resolved: the deepest existing ancestor
/// is canonicalised (a dangling symlink is refused, not followed), so nothing the tool writes can
/// land outside the addon content through a link.
pub fn inside_addon(addon_root: &Path, path: &Path) -> Result<(), CsError> {
    let root = std::fs::canonicalize(addon_root).map_err(|e| io(addon_root, e))?;
    let mut probe = path.to_path_buf();
    while std::fs::symlink_metadata(&probe).is_err() {
        if !probe.pop() {
            break;
        }
    }
    let real = std::fs::canonicalize(&probe).map_err(|e| io(&probe, e))?;
    if real.starts_with(&root) {
        Ok(())
    } else {
        Err(CsError::BadFile(format!(
            "{} resolves to {}, outside the addon content folder {}",
            path.display(),
            real.display(),
            root.display()
        )))
    }
}

fn io(path: &Path, e: std::io::Error) -> CsError {
    CsError::Io(format!("{}: {e}", path.display()))
}

/// Write `text` to `path` (parents created) through a sibling temp file and a rename.
pub fn write_file(path: &Path, text: &str) -> Result<(), CsError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(path, e)
    })
}

/// Forward-slash JSON string of a host path (the OUTPUT_DIR literal in rollup.config.js).
fn js_string(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string().replace('\\', "/")).expect("string")
}

/// The rollup.config.js of a new project with the given scripts.
pub fn render_rollup(out_dir: &Path, scripts: &[String]) -> String {
    let targets: Vec<String> = scripts.iter().map(|n| target_line(n)).collect();
    TPL_ROLLUP
        .replace("'__OUTPUT_DIR__'", &js_string(out_dir))
        .replace("__TARGETS__", &targets.join("\n"))
}

/// What one script's `tsconfig.json` contains (`override_active`: type-check against the game's
/// own point_script.d.ts, see [`types`]).
pub fn script_tsconfig(override_active: bool) -> String {
    if override_active {
        types::override_tsconfig_text(TPL_TSCONFIG)
    } else {
        TPL_TSCONFIG.to_string()
    }
}

/// The files of a new script, `(path relative to the project, text)`.
pub fn script_files(name: &str, override_active: bool) -> Vec<(String, String)> {
    vec![
        (format!("src/{name}/index.ts"), TPL_INDEX_TS.replace("__NAME__", name)),
        (format!("src/{name}/tsconfig.json"), script_tsconfig(override_active)),
    ]
}

/// Create the project files (no npm). Refuses an existing project. Returns the files written,
/// relative to the project.
pub fn scaffold(project: &Project, scripts: &[String]) -> Result<Vec<String>, CsError> {
    if project.exists() {
        return Err(CsError::Exists(project.dir.display().to_string()));
    }
    for name in scripts {
        check_name(name)?;
    }
    for (i, name) in scripts.iter().enumerate() {
        if scripts[..i].contains(name) {
            return Err(CsError::Duplicate(name.clone()));
        }
    }
    let pkg_name = package_name(&project.addon);
    let mut files: Vec<(String, String)> = vec![
        ("package.json".into(), TPL_PACKAGE_JSON.replace("__PROJECT_NAME__", &pkg_name)),
        ("rollup.config.js".into(), render_rollup(&project.out_dir, scripts)),
        (".gitignore".into(), TPL_GITIGNORE.to_string()),
        (
            "README.md".into(),
            TPL_README
                .replace("__PROJECT_NAME__", &pkg_name)
                .replace("__ADDON__", &project.addon)
                .replace("__UPSTREAM__", UPSTREAM_VERSION),
        ),
        ("LICENSE.create-cs-script".into(), TPL_LICENSE.to_string()),
    ];
    for name in scripts {
        files.extend(script_files(name, false));
    }
    inside_addon(&project.addon_root, &project.dir)?;
    inside_addon(&project.addon_root, &project.out_dir)?;
    std::fs::create_dir_all(&project.out_dir).map_err(|e| io(&project.out_dir, e))?;
    let mut written = Vec::new();
    for (rel, text) in files {
        write_file(&project.dir.join(&rel), &text)?;
        written.push(rel);
    }
    Ok(written)
}

/// Add a script: its folder and a rollup target. Refuses a duplicate. The tsconfig follows the
/// project's types override when one is active.
pub fn add_script(project: &Project, name: &str) -> Result<Vec<String>, CsError> {
    project.require()?;
    check_name(name)?;
    let rollup = std::fs::read_to_string(project.rollup_path()).map_err(|e| io(&project.rollup_path(), e))?;
    let updated = insert_target(&rollup, name)?; // Duplicate / Config errors before any write
    let dir = project.src_dir(name);
    inside_addon(&project.addon_root, &dir)?;
    if dir.exists() {
        return Err(CsError::Duplicate(format!("{name} (src/{name}/ exists without a rollup target)")));
    }
    let mut written = Vec::new();
    for (rel, text) in script_files(name, types::override_active(&project.dir)) {
        write_file(&project.dir.join(&rel), &text)?;
        written.push(rel);
    }
    write_file(&project.rollup_path(), &updated)?;
    written.push("rollup.config.js".into());
    Ok(written)
}

/// Re-insert only the rollup target of a script whose `src/<name>/` folder already exists (the
/// caller has established that the folder is its own, `player_model`'s marked file). Nothing in the
/// folder is touched.
pub fn add_target_only(project: &Project, name: &str) -> Result<(), CsError> {
    project.require()?;
    check_name(name)?;
    let rollup = std::fs::read_to_string(project.rollup_path()).map_err(|e| io(&project.rollup_path(), e))?;
    let updated = insert_target(&rollup, name)?;
    write_file(&project.rollup_path(), &updated)
}

/// The `.ts` files under `src/<name>/` (relative to it, sorted), without descending into
/// anything but plain directories.
pub fn list_sources(project: &Project, name: &str) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            let rel = if prefix.is_empty() { file_name.clone() } else { format!("{prefix}/{file_name}") };
            if kind.is_dir() && depth < 4 {
                walk(&entry.path(), &rel, out, depth + 1);
            } else if kind.is_file() && file_name.ends_with(".ts") {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(&project.src_dir(name), "", &mut out, 0);
    out.sort();
    out
}
