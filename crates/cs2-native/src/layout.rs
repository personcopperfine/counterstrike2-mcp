//! The bytes this launcher writes that are not copies of a build artefact: the Metamod vdf, the
//! `sdkenginetools.txt` edit, the gbe config files and `steam_interfaces.txt`.
//!
//! The vdf and the manifest edit are byte-identical to `cargo bridge install`'s
//! (`crates/cs2-bridge-tools/src/install/text.rs`: `WINE_VDF`, `LINUX_VDF`,
//! `apply_manifest_edit`), so a tree prepared here and a tree `cargo bridge install` prepared load
//! the bridge the same way.

/// `game/csgo/addons/metamod/cs2mcp.vdf` on Windows: relative to the mod dir (`game/csgo`), it
/// names the **tools** copy of the bridge, so Metamod and toolframework2 hold one image
/// (`cargo bridge install`, the Wine layout in its module doc).
pub const WINDOWS_VDF: &str =
    "\"Metamod Plugin\"\n{\n\t\"alias\"\t\t\"cs2mcp\"\n\t\"file\"\t\t\"../bin/win64/tools/cs2_bridge.dll\"\n}\n";

/// `csgo/addons/metamod/cs2mcp.vdf` on Linux (no tools manifest there; single tabs, the
/// live-verified byte form of `cargo bridge install`'s `LINUX_VDF`).
pub const LINUX_VDF: &str =
    "\"Metamod Plugin\"\n{\n\t\"alias\"\t\"cs2mcp\"\n\t\"file\"\t\"addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so\"\n}\n";

/// The marker `cargo bridge install` uses to tell an edited manifest.
const ENTRY_MARKER: &str = "m_Name = \"cs2_bridge\"";

/// Insert the `cs2_bridge` tool entry before the `hammer` entry of `game/bin/sdkenginetools.txt`
/// (`cargo bridge install`'s `apply_manifest_edit`: same anchor, same text, the file's own line
/// ending).
/// Idempotent. `None` when the hammer anchor is missing (Valve changed the file: refuse).
pub fn sdkenginetools_edit(text: &str) -> Option<String> {
    if text.contains(ENTRY_MARKER) {
        return Some(text.to_string());
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let anchor = format!("\t\t{{{eol}\t\t\tm_Name = \"hammer\"");
    if !text.contains(&anchor) {
        return None;
    }
    let entry = format!(
        "\t\t{{{eol}\t\t\tm_Name = \"cs2_bridge\"{eol}\t\t\tm_FriendlyName = \"CS2 Bridge (Rust)\"{eol}\t\t\tm_Library = \"tools/cs2_bridge.dll\"{eol}\t\t\tm_bShowInToolsMenu = false{eol}\t\t}},{eol}"
    );
    Some(text.replacen(&anchor, &(entry + &anchor), 1))
}

/// The inverse of [`sdkenginetools_edit`]: remove the one entry it inserts (with the file's own
/// line ending). `None` when the entry is not there in exactly that form.
pub fn sdkenginetools_unedit(text: &str) -> Option<String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let entry = format!(
        "\t\t{{{eol}\t\t\tm_Name = \"cs2_bridge\"{eol}\t\t\tm_FriendlyName = \"CS2 Bridge (Rust)\"{eol}\t\t\tm_Library = \"tools/cs2_bridge.dll\"{eol}\t\t\tm_bShowInToolsMenu = false{eol}\t\t}},{eol}"
    );
    text.contains(&entry).then(|| text.replacen(&entry, "", 1))
}

/// Does this `sdkenginetools.txt` carry our entry (in any form)?
pub fn has_bridge_entry(text: &str) -> bool {
    text.contains(ENTRY_MARKER)
}

/// gbe `steam_settings/configs.main.ini`: the container's settings (workshop tools install),
/// nothing else changed.
pub const GBE_CONFIGS_MAIN: &str = "# GBE main configuration for CS2 (appid 730), written by cs2mcp-native\n\
# Only non-default values are set; see the gbe_fork post_build docs for the full list of options.\n\
\n\
[main::general]\n\
# modern auth ticket format (required by current Source 2 games)\n\
new_app_ticket=1\n\
gc_token=1\n\
# fail SteamNetworking cert requests locally instead of awaiting a server\n\
# response (the game stalled forever on 'Retrying cert request' otherwise)\n\
disable_networking=1\n\
# a desktop install, not a Steam Deck / Proton session\n\
steam_hardware_type=0\n\
is_under_proton=0\n";

/// gbe `steam_settings/configs.user.ini`: a placeholder identity in Valve's range (the
/// container's), saves outside the game tree (`local_save_path`).
pub fn gbe_configs_user(save_dir: &str) -> String {
    format!(
        "# GBE user configuration for CS2 (appid 730), written by cs2mcp-native\n\
# The emulated account identity: a non-impersonating placeholder in Valve's range.\n\
\n\
[user::general]\n\
account_name=cs2mcp\n\
account_steamid=76561198000000001\n\
language=english\n\
ip_country=US\n\
\n\
[user::saves]\n\
local_save_path={save_dir}\n"
    )
}

/// `steam_appid.txt` / `steam_settings/steam_appid.txt`.
pub const STEAM_APPID: &str = "730\n";

/// Is this Steam API library already a Steam emulator? gbe_fork's builds (the Windows DLL and the
/// Linux `.so` alike) name their own settings file, `configs.main.ini`; Valve's library does not.
/// Then it
/// cannot be the source for `steam_interfaces.txt` (it carries every interface version), and in
/// `client` mode the real Steam client would never be reached.
pub fn is_gbe_library(bytes: &[u8]) -> bool {
    const MARK: &[u8] = b"configs.main.ini";
    bytes.windows(MARK.len()).any(|w| w == MARK)
}

/// `steam_settings/steam_interfaces.txt` from the game's **original** Steam API library, the way
/// gbe's `generate_interfaces` finds them: every `Steam…NNN` / `STEAM…NNN` interface version
/// string (letters and `_`, then exactly three digits, word-bounded), except the flat-API
/// accessor names (`SteamAPI_…`), and only the **first** `SteamClientNNN` (the library carries
/// an older and a newer one; gbe keeps the first). One per line, in file order.
///
/// Checked against build 2000940's `steam_api64.dll`: the same 28 names as the container's
/// generated file (unit test with a synthetic buffer below; the live comparison is
/// `cs2mcp-native gbe-interfaces <dll>`).
pub fn steam_interfaces(bytes: &[u8]) -> String {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out: Vec<String> = Vec::new();
    let mut have_client = false;
    let mut i = 0;
    while i + 5 <= bytes.len() {
        let head = &bytes[i..i + 5];
        let starts = head == b"Steam" || head == b"STEAM";
        if !starts || (i > 0 && word(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let mut j = i + 5;
        while j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
            j += 1;
        }
        let digits_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        let ok = j - digits_start == 3 && (j >= bytes.len() || !word(bytes[j]));
        if ok {
            let name = String::from_utf8_lossy(&bytes[i..j]).into_owned();
            let client = name.starts_with("SteamClient") && digits_start == i + "SteamClient".len();
            let skip = name.starts_with("SteamAPI_") || (client && have_client);
            if !skip && !out.contains(&name) {
                have_client |= client;
                out.push(name);
            }
        }
        i = j.max(i + 1);
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "\"ToolList\"\n{\n\tm_Tools =\n\t[\n\t\t{\n\t\t\tm_Name = \"hammer\"\n\t\t\tm_Library = \"tools/hammer.dll\"\n\t\t},\n\t]\n}\n";

    #[test]
    fn the_entry_goes_before_hammer_once() {
        let edited = sdkenginetools_edit(MANIFEST).unwrap();
        let expect = "\"ToolList\"\n{\n\tm_Tools =\n\t[\n\t\t{\n\t\t\tm_Name = \"cs2_bridge\"\n\t\t\tm_FriendlyName = \"CS2 Bridge (Rust)\"\n\t\t\tm_Library = \"tools/cs2_bridge.dll\"\n\t\t\tm_bShowInToolsMenu = false\n\t\t},\n\t\t{\n\t\t\tm_Name = \"hammer\"\n\t\t\tm_Library = \"tools/hammer.dll\"\n\t\t},\n\t]\n}\n";
        assert_eq!(edited, expect);
        assert_eq!(sdkenginetools_edit(&edited).unwrap(), edited);
        assert_eq!(sdkenginetools_unedit(&edited).unwrap(), MANIFEST);
        assert_eq!(sdkenginetools_unedit(MANIFEST), None);
        let crlf = MANIFEST.replace('\n', "\r\n");
        assert_eq!(sdkenginetools_unedit(&sdkenginetools_edit(&crlf).unwrap()).unwrap(), crlf);
    }

    #[test]
    fn crlf_files_get_crlf_entries_and_a_missing_anchor_refuses() {
        let crlf = MANIFEST.replace('\n', "\r\n");
        let edited = sdkenginetools_edit(&crlf).unwrap();
        assert!(edited.contains("\t\t\tm_Name = \"cs2_bridge\"\r\n\t\t\tm_FriendlyName"));
        assert!(!edited.replace("\r\n", "").contains('\n'));
        assert_eq!(sdkenginetools_edit("\"ToolList\"\n{\n}\n"), None);
    }

    #[test]
    fn interface_scan_keeps_versions_drops_flat_api_and_the_second_client() {
        let mut b = Vec::new();
        for s in [
            "SteamClient017",
            "SteamClient023",
            "SteamAPI_SteamUser_v023",
            "SteamUser023",
            "STEAMHTMLSURFACE_INTERFACE_VERSION_005",
            "STEAMINVENTORY_INTERFACE_V003",
            "SteamUser023",
            "xSteamFriends018",
            "SteamFriends0189",
            "SteamNetworkingSockets012",
        ] {
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        }
        assert_eq!(
            steam_interfaces(&b),
            "SteamClient017\nSteamUser023\nSTEAMHTMLSURFACE_INTERFACE_VERSION_005\nSTEAMINVENTORY_INTERFACE_V003\nSteamNetworkingSockets012\n"
        );
        assert!(!is_gbe_library(&b));
        assert!(is_gbe_library(b"..steam_settings/configs.main.ini.."));
    }
}
