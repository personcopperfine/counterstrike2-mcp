//! `new_map`'s template assets: the resources Valve's `addon_template` map references that live in
//! the template ADDON (not in the game's VPKs), copied into the new map's addon so the copy is
//! self-contained. Never writes into the template addon, never replaces an existing file (every
//! write is [`write_atomic_bytes_new`]); whatever cannot be brought along is reported as `missing`
//! with the reason, so the caller (and the compile log) holds no surprise.
//!
//! * `.vpost` post-process files named in the map (`postprocess/basic_linear_post.vpost`): copied
//!   into `content/`, and the compiled `_c` into `game/` when the template addon has one and the
//!   new addon's `game/csgo_addons/<addon>` exists.
//! * soundscape events (`env_soundscape` names such as `ambient_example.outdoors`) are defined only
//!   in the template addon's `soundevents/soundevents_addon.vsndevts`: that file is copied only when
//!   the new addon has no `soundevents_addon.vsndevts` yet (an existing one is never merged or
//!   replaced). The `.vsnd` files its events reference are not copied.
use crate::vmdl::fsutil::write_atomic_bytes_new;
use serde_json::{json, Value};
use std::path::{Component, Path};

const SOUNDEVENTS: &str = "soundevents/soundevents_addon.vsndevts";
const SOUNDSCAPE_BASE: &str = "amb.soundscapeParent.base";

/// What [`bring_template_assets`] did: `{copied, missing, alreadyPresent}` (arrays of objects).
pub fn bring_template_assets(app_root: &Path, addon: &str, template_bytes: &[u8]) -> Value {
    let tpl_content = app_root.join("content/csgo_addons").join(crate::tools::new_map::TEMPLATE_ADDON);
    let tpl_game = app_root.join("game/csgo_addons").join(crate::tools::new_map::TEMPLATE_ADDON);
    let dst_content = app_root.join("content/csgo_addons").join(addon);
    let dst_game = app_root.join("game/csgo_addons").join(addon);
    let mut copied: Vec<Value> = Vec::new();
    let mut missing: Vec<Value> = Vec::new();
    let mut present: Vec<Value> = Vec::new();

    for reference in references_with_ext(template_bytes, ".vpost") {
        let source = tpl_content.join(&reference);
        let dest = dst_content.join(&reference);
        if exists(&dest) {
            present.push(json!({"ref": reference, "kind": "postprocess"}));
            continue;
        }
        if !source.is_file() {
            missing.push(json!({"ref": reference, "kind": "postprocess",
                "reason": "not in the template addon (a game-VPK resource, or absent): nothing to copy"}));
            continue;
        }
        match copy_new(&source, &dest) {
            Ok(true) => copied.push(json!({"ref": reference, "kind": "postprocess", "to": format!("content/{reference}")})),
            Ok(false) => present.push(json!({"ref": reference, "kind": "postprocess"})),
            Err(error) => {
                missing.push(json!({"ref": reference, "kind": "postprocess", "reason": format!("copy failed: {error}")}));
                continue;
            }
        }
        // the compiled twin, "the same way the template addon has it"
        let compiled = format!("{reference}_c");
        let compiled_source = tpl_game.join(&compiled);
        if compiled_source.is_file() {
            if !dst_game.is_dir() {
                missing.push(json!({"ref": compiled, "kind": "postprocess (compiled)",
                    "reason": format!("game/csgo_addons/{addon} does not exist yet: compile_asset builds it")}));
            } else {
                match copy_new(&compiled_source, &dst_game.join(&compiled)) {
                    Ok(true) => copied.push(json!({"ref": compiled, "kind": "postprocess (compiled)", "to": format!("game/{compiled}")})),
                    Ok(false) => present.push(json!({"ref": compiled, "kind": "postprocess (compiled)"})),
                    Err(error) => missing.push(json!({"ref": compiled, "kind": "postprocess (compiled)", "reason": format!("copy failed: {error}")})),
                }
            }
        }
    }

    // soundscapes
    if let Ok(text) = std::fs::read_to_string(tpl_content.join(SOUNDEVENTS)) {
        let used: Vec<String> = soundscape_parents(&text)
            .into_iter()
            .filter(|name| contains(template_bytes, name.as_bytes()))
            .collect();
        if !used.is_empty() {
            let dest = dst_content.join(SOUNDEVENTS);
            let skip = |reason: String| {
                json!({"ref": used, "kind": "soundscape", "file": SOUNDEVENTS, "reason": reason})
            };
            if std::fs::symlink_metadata(&dest).is_ok() {
                missing.push(skip(format!(
                    "the addon already has {SOUNDEVENTS}: it is not overwritten or merged (add the \
                     events with edit_soundevents); the soundscapes stay undefined until then"
                )));
            } else {
                match copy_new(&tpl_content.join(SOUNDEVENTS), &dest) {
                    Ok(true) => {
                        copied.push(json!({"ref": SOUNDEVENTS, "kind": "soundscape", "events": used,
                            "to": format!("content/{SOUNDEVENTS}"),
                            "note": "run compile_soundevents to build the .vsndevts_c"}));
                        let sounds = vsnd_refs(&text);
                        if !sounds.is_empty() {
                            missing.push(json!({"ref": "sounds/*.vsnd", "kind": "sounds", "count": sounds.len(),
                                "examples": sounds.iter().take(5).collect::<Vec<_>>(),
                                "reason": "the copied events reference these sounds; they are not copied \
                                           (the template addon's own wavs are not carried over), so those \
                                           tracks stay silent"}));
                        }
                    }
                    Ok(false) => missing.push(skip(format!("{SOUNDEVENTS} appeared meanwhile: not overwritten"))),
                    Err(error) => missing.push(skip(format!("copy failed: {error}"))),
                }
            }
        }
    }
    json!({"copied": copied, "missing": missing, "alreadyPresent": present})
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Copy `source` to `dest` as a new file (parents created); `Ok(false)` when `dest` exists.
fn copy_new(source: &Path, dest: &Path) -> std::io::Result<bool> {
    let bytes = std::fs::read(source)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match write_atomic_bytes_new(dest, &bytes) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Distinct relative resource paths ending in `ext` (case-insensitive) found as printable runs in
/// `bytes`, sorted. Only plain relative paths survive (no `..`, no root, no empty segment).
fn references_with_ext(bytes: &[u8], ext: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    let mut run: Vec<u8> = Vec::new();
    for &b in bytes.iter().chain(std::iter::once(&0u8)) {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'.' | b'-') {
            run.push(b);
            continue;
        }
        if run.len() > ext.len() && run.len() <= 260 {
            let text = String::from_utf8_lossy(&run).to_string();
            if text.to_ascii_lowercase().ends_with(ext) && plain_relative(&text) {
                found.insert(text);
            }
        }
        run.clear();
    }
    found.into_iter().collect()
}

fn plain_relative(path: &str) -> bool {
    !path.starts_with('/')
        && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)))
        && !path.split('/').any(str::is_empty)
}

/// Top-level events of a `.vsndevts` whose `base` is the soundscape parent.
fn soundscape_parents(text: &str) -> Vec<String> {
    let mut parents = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.starts_with('\t') && !line.starts_with("\t\t") && line.trim_start().starts_with('"') {
            let trimmed = line.trim();
            current = trimmed
                .strip_prefix('"')
                .and_then(|rest| rest.split_once('"'))
                .filter(|(_, tail)| tail.trim_start().starts_with('='))
                .map(|(name, _)| name.to_string());
        } else if let Some(name) = &current {
            let trimmed = line.trim();
            if trimmed.starts_with("base") && trimmed.contains(&format!("\"{SOUNDSCAPE_BASE}\"")) {
                parents.push(name.clone());
                current = None;
            }
        }
    }
    parents
}

/// Distinct `"sounds/....vsnd"` strings of an event file.
fn vsnd_refs(text: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    for piece in text.split('"') {
        if piece.ends_with(".vsnd") && piece.starts_with("sounds/") {
            found.insert(piece.to_string());
        }
    }
    found.into_iter().collect()
}

