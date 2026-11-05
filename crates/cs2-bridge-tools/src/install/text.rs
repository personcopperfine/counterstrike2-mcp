//! The bytes install writes that are not copies of a build artefact: the two Metamod vdfs and the
//! `sdkenginetools.txt` entry, plus the anchor fingerprint ensure holds the manifest to. One
//! producer each: install writes exactly these bytes and ensure compares against the same
//! function, so an install can never leave a file ensure reads as stale. `crates/cs2-native`
//! (`layout.rs`) writes the same vdf and manifest bytes for a native install.

/// `game/csgo/addons/metamod/cs2mcp.vdf` on Wine. Metamod 2.0's format: the core reads the
/// `"Metamod Plugin"` keys `file` (+ optional `alias`) from every vdf in `<mod>/addons/metamod`
/// (`core/provider/source2/provider_source2.cpp:170`, `core/metamod.cpp:483`), and resolves a
/// relative `file` against the mod dir `game/csgo` (`core/metamod.cpp:1103-1150`). So
/// `../bin/win64/tools/cs2_bridge.dll` is the very tool DLL the manifest entry loads: Metamod's
/// provider dlopens it first and toolframework2's load refcounts the same module - one image.
/// That is also why install never puts a second copy under `addons/cs2mcp` on Wine.
pub const WINE_VDF: &str =
    "\"Metamod Plugin\"\n{\n\t\"alias\"\t\t\"cs2mcp\"\n\t\"file\"\t\t\"../bin/win64/tools/cs2_bridge.dll\"\n}\n";

/// `csgo/addons/metamod/cs2mcp.vdf` on Linux: no tools manifest there, so Metamod loads the
/// bridge straight out of the addons copy (single tabs: the live-verified byte form).
pub const LINUX_VDF: &str =
    "\"Metamod Plugin\"\n{\n\t\"alias\"\t\"cs2mcp\"\n\t\"file\"\t\"addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so\"\n}\n";

/// The text that marks the manifest as carrying our entry.
pub const ENTRY_MARKER: &str = "m_Name = \"cs2_bridge\"";

/// The default anchor fingerprint (normalised) when no backup recorded one.
pub const DEFAULT_MANIFEST_ANCHOR: &str = "m_Name = \"hammer\"";

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn hammer_anchor(eol: &str) -> String {
    format!("\t\t{{{eol}\t\t\tm_Name = \"hammer\"")
}

/// Our entry, in the file's own line ending.
fn entry(eol: &str) -> String {
    format!(
        "\t\t{{{eol}\t\t\tm_Name = \"cs2_bridge\"{eol}\t\t\tm_FriendlyName = \"CS2 Bridge (Rust)\"{eol}\t\t\tm_Library = \"tools/cs2_bridge.dll\"{eol}\t\t\tm_bShowInToolsMenu = false{eol}\t\t}},{eol}"
    )
}

/// THE manifest edit: insert our entry before the hammer entry (idempotent: a file that already
/// carries the marker comes back unchanged). `None`: the hammer anchor is missing (Valve changed
/// the layout; refuse rather than guess). ensure calls this on the current text and compares:
/// "current" means the file is what install's edit would produce from it.
pub fn apply_manifest_edit(text: &str) -> Option<String> {
    if text.contains(ENTRY_MARKER) {
        return Some(text.to_string());
    }
    let eol = eol_of(text);
    let anchor = hammer_anchor(eol);
    if !text.contains(&anchor) {
        return None;
    }
    Some(text.replacen(&anchor, &format!("{}{anchor}", entry(eol)), 1))
}

/// What uninstall's revert of the manifest edit found.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// our exact entry removed: the new text
    Removed(String),
    /// no marker: already gone (idempotent)
    Absent,
    /// the marker is there but not in the exact form install wrote: refuse (never restore a
    /// stale Valve file from the backup instead)
    Foreign,
}

/// Remove exactly our entry from the CURRENT manifest text (a Steam update's other changes stay).
pub fn remove_manifest_entry(text: &str) -> Removal {
    if !text.contains(ENTRY_MARKER) {
        return Removal::Absent;
    }
    let ours = entry(eol_of(text));
    let out = text.replacen(&ours, "", 1);
    if out.contains(ENTRY_MARKER) {
        Removal::Foreign
    } else {
        Removal::Removed(out)
    }
}

/// The anchor comparison is normalised: `\r` stripped, surrounding whitespace trimmed, runs of
/// blanks collapsed to one space. A reformatted `m_Name    =   "hammer"` is still the anchor.
pub fn norm(s: &str) -> String {
    let s = s.replace('\r', "");
    let mut out = String::with_capacity(s.len());
    let mut blank = false;
    for c in s.trim().chars() {
        if c == ' ' || c == '\t' {
            if !blank {
                out.push(' ');
            }
            blank = true;
        } else {
            out.push(c);
            blank = false;
        }
    }
    out
}

/// The first wanted anchor that is not a normalised line of `text` (blank and `//` comment lines
/// ignored); `None` when every anchor is still there.
pub fn anchors_missing(text: &str, wants: &[String]) -> Option<String> {
    let lines: Vec<String> = text
        .lines()
        .map(norm)
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .collect();
    wants.iter().map(|w| norm(w)).find(|w| !lines.contains(w))
}

/// The fingerprint install records in the backup's ANCHORS: the first line holding
/// `m_Name<ws>=<ws>"hammer"`, stripped of surrounding whitespace.
pub fn manifest_anchor_line(text: &str) -> Option<String> {
    text.split('\n')
        .find(|line| has_hammer_name(line))
        .map(|line| {
            line.trim_matches(|c: char| c.is_ascii_whitespace())
                .to_string()
        })
        .filter(|l| !l.is_empty())
}

fn has_hammer_name(line: &str) -> bool {
    let mut rest = line;
    while let Some(i) = rest.find("m_Name") {
        let after =
            rest[i + "m_Name".len()..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if let Some(v) = after.strip_prefix('=') {
            if v.trim_start_matches(|c: char| c.is_ascii_whitespace())
                .starts_with("\"hammer\"")
            {
                return true;
            }
        }
        rest = &rest[i + 1..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const LF: &str =
        "{\n\tm_EngineTools =\n\t[\n\t\t{\n\t\t\tm_Name = \"hammer\"\n\t\t},\n\t]\n}\n";

    #[test]
    fn the_edit_inserts_before_hammer_and_is_idempotent() {
        let once = apply_manifest_edit(LF).unwrap();
        assert!(once.contains("\t\t{\n\t\t\tm_Name = \"cs2_bridge\"\n"));
        assert!(once.find("cs2_bridge").unwrap() < once.find("\"hammer\"").unwrap());
        assert_eq!(apply_manifest_edit(&once).unwrap(), once);
        assert_eq!(
            remove_manifest_entry(&once),
            Removal::Removed(LF.to_string())
        );
        assert_eq!(remove_manifest_entry(LF), Removal::Absent);
    }

    #[test]
    fn crlf_files_get_a_crlf_entry() {
        let crlf = LF.replace('\n', "\r\n");
        let once = apply_manifest_edit(&crlf).unwrap();
        assert!(once.contains("m_Name = \"cs2_bridge\"\r\n"));
        assert!(!once.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn no_anchor_refuses_and_a_reformatted_entry_is_foreign() {
        assert_eq!(apply_manifest_edit("{\n}\n"), None);
        let odd = "\t\t{\n\t\t\tm_Name = \"cs2_bridge\"\n\t\t},\n";
        assert_eq!(remove_manifest_entry(odd), Removal::Foreign);
    }

    #[test]
    fn anchors_are_normalised() {
        let text = "\t\t\tm_Name     =    \"hammer\"\r\n// comment\n\n";
        assert_eq!(anchors_missing(text, &["m_Name = \"hammer\"".into()]), None);
        assert_eq!(
            anchors_missing("{\n}\n", &["m_Name = \"hammer\"".into()]),
            Some("m_Name = \"hammer\"".into())
        );
        assert_eq!(
            manifest_anchor_line(LF).as_deref(),
            Some("m_Name = \"hammer\"")
        );
        assert_eq!(
            manifest_anchor_line("x\n\tm_Name=\"hammer\"\r\n").as_deref(),
            Some("m_Name=\"hammer\"")
        );
        assert_eq!(manifest_anchor_line("m_Name = \"hammer2\""), None);
    }
}
