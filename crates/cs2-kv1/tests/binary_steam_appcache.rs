//! Steam's containers around binary KV1: `shortcuts.vdf` (a bare document), `appinfo.vdf` and
//! `packageinfo.vdf` (a wrapper around one document per app or package). Read-only, and gated on
//! the file being present, exactly like `crates/cs2-kv3/tests/real_files.rs`: without a Steam install
//! every test here prints `skipped: … missing` and passes.
//!
//! The install is `$CS2_KV1_STEAM_DIR` when that is set, `$HOME/.steam/steam` otherwise. The
//! override is how a checkout can point the container tests at a copy of a real appcache (for a
//! Wine prefix, a container volume, …) without adding game data to the repository.
//!
//! Nothing here prints file contents: the tests report counts, versions and entry metadata only.
//!
//! # The wrapper layouts
//!
//! Learned from `xPaw/SteamAppInfo` (`AppInfo.cs`, `PackageInfo.cs`, MIT) and then verified
//! against the real files this port was developed on: the magic's low byte is the version, every
//! entry's `size` lands exactly on the next entry, each embedded document consumes its entry down
//! to the last byte, and the v41 string table ends at the end of the file. Older Steam versions
//! (the historical 27/28/29 numbering) are not supported: nothing here has been verified for them,
//! so a file with another version is skipped loudly by the test rather than parsed on a guess.
//!
//! `appinfo.vdf`, versions 39..=41 (41 is what current Steam writes):
//!
//! ```text
//! u32   magic                 0x07564400 | version
//! u32   universe
//! u64   string_table_offset   version >= 41
//! repeat until appid == 0:
//!   u32   appid
//!   u32   size                bytes from here to the end of this entry
//!   u32   info_state
//!   u32   last_updated        unix time
//!   u64   token
//!   u8[20] hash
//!   u32   change_number
//!   u8[20] binary_data_hash   version >= 40
//!   u8[]  document            `size` minus the fields above
//! u32   appid = 0
//! u32   string_count          version >= 41, at string_table_offset
//!       string_count NUL-terminated UTF-8 keys
//! ```
//!
//! Every key of a version 41 document — the root key included — is an `int32` index into that
//! string table, so the table has to be read before the entries are parsed.
//!
//! `packageinfo.vdf`, versions 39..=40 (40 is what current Steam writes):
//!
//! ```text
//! u32   magic                 0x06565500 | version
//! u32   universe
//! repeat until subid == 0xFFFFFFFF:
//!   u32   subid
//!   u8[20] hash
//!   u32   change_number
//!   u64   token                version >= 40
//!   u8[]  document             no length: the document's own end marker frames it
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use cs2_kv1::binary::{self, ReadOptions, StringTable};
use cs2_kv1::{Document, Error, Value};

/// The Steam install to look at: `$CS2_KV1_STEAM_DIR`, or `$HOME/.steam/steam`.
fn steam_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CS2_KV1_STEAM_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home).join(".steam").join("steam")
}

/// The bytes of a file under the Steam install, or `None` (with a loud line) when it is absent.
fn steam_file(relative: &str) -> Option<Vec<u8>> {
    let path = steam_dir().join(relative);
    match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            println!("skipped: {} missing ({error})", path.display());
            None
        }
    }
}

/// Every `userdata/<account>/config/shortcuts.vdf`, sorted for a stable test order.
fn shortcuts_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let userdata = steam_dir().join("userdata");
    let Ok(accounts) = fs::read_dir(&userdata) else {
        println!("skipped: {} missing", userdata.display());
        return files;
    };
    for account in accounts.flatten() {
        let candidate = account.path().join("config").join("shortcuts.vdf");
        if candidate.is_file() {
            files.push(candidate);
        } else {
            println!("skipped: {} missing", candidate.display());
        }
    }
    files.sort();
    files
}

/// Why a container could not be walked.
enum WalkError {
    /// A version this port has not verified against a real file: the test skips loudly.
    Unsupported(String),
    /// A file that claims a supported version but does not match the layout: a test failure.
    Malformed(String),
}

impl WalkError {
    fn unsupported(message: impl Into<String>) -> Self {
        WalkError::Unsupported(message.into())
    }
}

/// Unwraps a walk result in a test that cannot skip: both kinds of failure are test failures here.
fn walk<T>(result: Result<T, WalkError>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(WalkError::Unsupported(why)) => panic!("{what}: {why}"),
        Err(WalkError::Malformed(why)) => panic!("{what}: {why}"),
    }
}

/// A read cursor over the container bytes, with the bounds checks the layouts above need.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        let mut value = [0u8; 8];
        value.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(value))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        if length > self.bytes.len() - self.pos {
            return Err(format!("entry at {} wants {length} bytes, {} left", self.pos, self.bytes.len() - self.pos));
        }
        let slice = &self.bytes[self.pos..self.pos + length];
        self.pos += length;
        Ok(slice)
    }

    fn skip(&mut self, length: usize) -> Result<(), String> {
        self.take(length).map(|_| ())
    }

    fn seek(&mut self, pos: usize) -> Result<(), String> {
        if pos > self.bytes.len() {
            return Err(format!("offset {pos} is past the end of the file ({})", self.bytes.len()));
        }
        self.pos = pos;
        Ok(())
    }
}

/// One app of an `appinfo.vdf`.
struct AppEntry<'a> {
    appid: u32,
    /// The embedded document, exactly the bytes the entry's `size` covers.
    document: &'a [u8],
}

/// A parsed `appinfo.vdf` container.
struct AppInfo<'a> {
    version: u32,
    /// Version 41 keys live here; None for versions 39 and 40, whose keys are inline strings.
    string_table: Option<StringTable>,
    apps: Vec<AppEntry<'a>>,
}

/// Walks the `appinfo.vdf` wrapper without interpreting the entries' documents.
fn parse_appinfo(bytes: &[u8]) -> Result<AppInfo<'_>, WalkError> {
    let mut cursor = Cursor::new(bytes);
    let magic = cursor.u32().map_err(WalkError::Malformed)?;
    let version = magic & 0xFF;
    if magic >> 8 != 0x07_56_44 {
        return Err(WalkError::Malformed(format!("not an appinfo file: magic {magic:#010X}")));
    }
    if !(39..=41).contains(&version) {
        return Err(WalkError::unsupported(format!(
            "appinfo version {version} is outside the verified 39..=41 range"
        )));
    }
    let _universe = cursor.u32().map_err(WalkError::Malformed)?;

    let string_table_offset = if version >= 41 {
        Some(cursor.u64().map_err(WalkError::Malformed)? as usize)
    } else {
        None
    };

    let mut apps = Vec::new();
    loop {
        let appid = cursor.u32().map_err(WalkError::Malformed)?;
        if appid == 0 {
            break;
        }
        let size = cursor.u32().map_err(WalkError::Malformed)? as usize;
        let header = if version >= 40 { 60 } else { 40 };
        if size < header {
            return Err(WalkError::Malformed(format!("appid {appid}: entry size {size} is smaller than its header")));
        }
        cursor.skip(header).map_err(WalkError::Malformed)?;
        let document = cursor.take(size - header).map_err(WalkError::Malformed)?;
        apps.push(AppEntry { appid, document });
    }

    let string_table = match string_table_offset {
        // The table sits at the end of the file; the entries in front of it have been parsed.
        Some(offset) => {
            cursor.seek(offset).map_err(WalkError::Malformed)?;
            let count = cursor.u32().map_err(WalkError::Malformed)? as usize;
            let mut keys = Vec::new();
            for _ in 0..count {
                let rest = &bytes[cursor.pos..];
                let end = rest
                    .iter()
                    .position(|&byte| byte == 0)
                    .ok_or_else(|| WalkError::Malformed("unterminated string table".to_string()))?;
                keys.push(String::from_utf8_lossy(&rest[..end]).into_owned());
                cursor.pos += end + 1;
            }
            if cursor.pos != bytes.len() {
                return Err(WalkError::Malformed(format!(
                    "{} bytes after the end of the string table",
                    bytes.len() - cursor.pos
                )));
            }
            Some(StringTable::read_only(keys))
        }
        // Versions 39 and 40 carry their keys inline, so the entries are followed by the file end.
        None => None,
    };

    Ok(AppInfo { version, string_table, apps })
}

/// A package entry: its subid, the bytes of its document, and the parsed document.
type PackageEntry<'a> = (u32, &'a [u8], Document);

/// Parses every embedded document of a `packageinfo.vdf`, which has no length field per entry:
/// the document's own end marker says where the next `subid` starts.
///
/// Returns the version, then one [`PackageEntry`] per package.
fn parse_packageinfo(bytes: &[u8]) -> Result<(u32, Vec<PackageEntry<'_>>), WalkError> {
    let mut cursor = Cursor::new(bytes);
    let magic = cursor.u32().map_err(WalkError::Malformed)?;
    let version = magic & 0xFF;
    if magic >> 8 != 0x06_56_55 {
        return Err(WalkError::Malformed(format!("not a packageinfo file: magic {magic:#010X}")));
    }
    if !(39..=40).contains(&version) {
        return Err(WalkError::unsupported(format!(
            "packageinfo version {version} is outside the verified 39..=40 range"
        )));
    }
    let _universe = cursor.u32().map_err(WalkError::Malformed)?;

    let mut packages = Vec::new();
    loop {
        let subid = cursor.u32().map_err(WalkError::Malformed)?;
        if subid == 0xFFFF_FFFF {
            break;
        }
        let header = if version >= 40 { 32 } else { 24 };
        cursor.skip(header).map_err(WalkError::Malformed)?;
        match binary::parse_prefix(&bytes[cursor.pos..]) {
            Ok((document, used)) => {
                let document_bytes = cursor.take(used).map_err(WalkError::Malformed)?;
                packages.push((subid, document_bytes, document));
            }
            Err(error) => return Err(WalkError::Malformed(format!("subid {subid}: {error}"))),
        }
    }
    if cursor.pos != bytes.len() {
        return Err(WalkError::Malformed(format!("{} bytes after the end of the package list", bytes.len() - cursor.pos)));
    }
    Ok((version, packages))
}

/// The root object of a document, for assertions: every document in these containers is one, so
/// anything else is a test failure.
fn root_object(document: &Document) -> &cs2_kv1::Object {
    document.value.as_object().unwrap_or_else(|| panic!("{} is not an object", document.key))
}

#[test]
fn appinfo_walker_reads_a_synthetic_version_41_file() {
    // Builds a wrapper around a document this crate wrote with a string table, so the walker (and
    // the string-table path it depends on) is covered even on a machine without Steam.
    let mut table = StringTable::from_values(["appinfo", "appid"]);
    let document = cs2_kv1::KeyValue {
        key: "appinfo".to_string(),
        value: Value::Object(cs2_kv1::Object {
            entries: vec![cs2_kv1::KeyValue {
                key: "appid".to_string(),
                value: Value::Int32(42),
                condition: None,
            }],
        }),
        condition: None,
    };
    let blob = binary::write_with(&document, &mut binary::WriteOptions::with_table(&mut table))
        .expect("a writable table");
    assert_eq!(table.to_vec(), vec!["appinfo", "appid"], "no keys were added");

    let mut file = Vec::new();
    file.extend_from_slice(&0x0756_4429u32.to_le_bytes());
    file.extend_from_slice(&1u32.to_le_bytes());
    let table_offset_at = file.len();
    file.extend_from_slice(&0u64.to_le_bytes()); // patched below
    file.extend_from_slice(&42u32.to_le_bytes()); // appid
    let size_at = file.len();
    file.extend_from_slice(&0u32.to_le_bytes()); // patched below
    file.extend_from_slice(&1u32.to_le_bytes()); // info_state
    file.extend_from_slice(&0u32.to_le_bytes()); // last_updated
    file.extend_from_slice(&0u64.to_le_bytes()); // token
    file.extend_from_slice(&[0xAAu8; 20]); // hash
    file.extend_from_slice(&7u32.to_le_bytes()); // change_number
    file.extend_from_slice(&[0xBBu8; 20]); // binary data hash
    let blob_at = file.len();
    file.extend_from_slice(&blob);
    file.extend_from_slice(&0u32.to_le_bytes()); // end of the app list
    let table_offset = file.len();
    file[table_offset_at..table_offset_at + 8].copy_from_slice(&(table_offset as u64).to_le_bytes());
    file.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for key in table.iter() {
        file.extend_from_slice(key.as_bytes());
        file.push(0);
    }
    let size = (blob_at + blob.len() - (size_at + 4)) as u32;
    file[size_at..size_at + 4].copy_from_slice(&size.to_le_bytes());

    let appinfo = walk(parse_appinfo(&file), "the synthetic appinfo file");
    assert_eq!(appinfo.version, 41);
    assert_eq!(appinfo.apps.len(), 1);
    assert_eq!(appinfo.apps[0].appid, 42);
    assert_eq!(appinfo.apps[0].document, &blob[..], "the entry's size frames the document");

    let table = appinfo.string_table.as_ref().expect("version 41 has a string table");
    let options = ReadOptions::with_table(table);
    let (parsed, used) = binary::parse_prefix_with(appinfo.apps[0].document, &options).expect("the document");
    assert_eq!(used, blob.len());
    assert_eq!(parsed.key, "appinfo");
    assert_eq!(root_object(&parsed).get("appid"), Some(&Value::Int32(42)));

    // A wrapper that promises more than it holds is a walker error, not a panic.
    assert!(parse_appinfo(&file[..file.len() - 3]).is_err(), "a truncated string table");
    assert!(parse_appinfo(&file[..blob_at + 4]).is_err(), "a truncated entry");
    let mut wild = file.clone();
    wild[size_at..size_at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    assert!(parse_appinfo(&wild).is_err(), "an entry size past the end of the file");
}

#[test]
fn appinfo_embedded_documents() {
    let Some(bytes) = steam_file("appcache/appinfo.vdf") else { return };
    let appinfo = match parse_appinfo(&bytes) {
        Ok(appinfo) => appinfo,
        Err(WalkError::Unsupported(why)) => {
            println!("skipped: {why}");
            return;
        }
        Err(WalkError::Malformed(why)) => panic!("appinfo: {why}"),
    };

    let options = match &appinfo.string_table {
        Some(table) => ReadOptions::with_table(table),
        None => ReadOptions::default(),
    };
    // The writer half is exercised on the same real documents: rebuilding each one with the file's
    // own string table has to reproduce its bytes exactly.
    let mut table = appinfo.string_table.clone();
    let mut documents = 0;
    for entry in &appinfo.apps {
        let (document, used) = binary::parse_prefix_with(entry.document, &options)
            .unwrap_or_else(|error| panic!("appid {}: {error}", entry.appid));
        // The entry's `size` has to cover the document exactly.
        assert_eq!(used, entry.document.len(), "appid {}", entry.appid);
        // Every appinfo document is named "appinfo" and carries its own appid, which is what
        // proves the string-table keys resolve to the right strings.
        assert_eq!(document.key, "appinfo", "appid {}", entry.appid);
        match root_object(&document).get("appid") {
            Some(Value::Int32(appid)) => {
                assert_eq!(*appid as u32, entry.appid, "the document's appid");
            }
            other => panic!("appid {}: expected an appid child, got {other:?}", entry.appid),
        }

        let written = binary::write_with(&document, &mut binary::WriteOptions {
            string_table: table.as_mut(),
        })
        .unwrap_or_else(|error| panic!("appid {}: {error}", entry.appid));
        assert!(written == entry.document, "appid {}: the rewritten document differs", entry.appid);
        documents += 1;
    }
    assert_eq!(documents, appinfo.apps.len());
    println!(
        "appinfo.vdf version {}: {} apps, {documents} embedded documents parsed and rewritten byte for byte",
        appinfo.version,
        appinfo.apps.len()
    );
}

#[test]
fn packageinfo_embedded_documents() {
    let Some(bytes) = steam_file("appcache/packageinfo.vdf") else { return };
    let (version, packages) = match parse_packageinfo(&bytes) {
        Ok(parsed) => parsed,
        Err(WalkError::Unsupported(why)) => {
            println!("skipped: {why}");
            return;
        }
        Err(WalkError::Malformed(why)) => panic!("packageinfo: {why}"),
    };

    for (subid, document_bytes, document) in &packages {
        // A packageinfo document is named after its subid and repeats it in "packageid".
        assert_eq!(document.key, subid.to_string(), "subid {subid}");
        match root_object(document).get("packageid") {
            Some(Value::Int32(packageid)) => assert_eq!(*packageid as u32, *subid, "the document's packageid"),
            other => panic!("subid {subid}: expected a packageid child, got {other:?}"),
        }
        let written = binary::write(document).expect("an inline document fits the wire format");
        assert!(written == *document_bytes, "subid {subid}: the rewritten document differs");
    }
    println!(
        "packageinfo.vdf version {version}: {} packages, documents parsed and rewritten byte for byte",
        packages.len()
    );
}

#[test]
fn shortcuts_round_trip_byte_for_byte() {
    let files = shortcuts_files();
    if files.is_empty() {
        println!("skipped: no shortcuts.vdf under {}", steam_dir().join("userdata").display());
        return;
    }

    let mut identical = 0;
    for path in &files {
        let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let document = binary::parse(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(document.key, "shortcuts", "{}: the root key", path.display());
        let written = binary::write(&document).expect("a shortcuts document fits the wire format");
        if written == bytes {
            identical += 1;
            continue;
        }
        // Steam is known to leave bytes behind the document in some of its own files; those are
        // reported, everything up to the document's end still has to match exactly.
        assert!(
            written == bytes[..written.len()],
            "{}: the rewritten document differs from the file ({} vs {} bytes)",
            path.display(),
            written.len(),
            bytes.len()
        );
        println!("{}: {} trailing bytes after the document, not part of it", path.display(), bytes.len() - written.len());
    }
    println!("{} shortcuts.vdf files parsed and written byte for byte", identical);
}

#[test]
fn shortcuts_rejects_a_truncated_file() {
    // The negative half of the round trip: a shortcuts.vdf cut short must be an error, not a panic
    // and not a half-parsed document.
    let files = shortcuts_files();
    if files.is_empty() {
        println!("skipped: no shortcuts.vdf under {}", steam_dir().join("userdata").display());
        return;
    }
    for path in &files {
        let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let (_, used) = binary::parse_prefix(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        for cut in [used / 2, used - 1] {
            assert!(
                matches!(binary::parse(&bytes[..cut]), Err(Error::Binary { .. })),
                "{}: a {cut}-byte prefix parsed",
                path.display()
            );
        }
    }
    println!("{} truncated shortcuts.vdf files rejected", files.len());
}
