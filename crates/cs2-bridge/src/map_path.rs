//! `map_path.h` in Rust: the pure helpers of `map.open` (m2.16).
//!
//! # What map.open needs from these
//!
//! `map.open` takes a path addon-content-relative (`maps/x.vmap`): the form Hammer's own
//! `CBaseEditorApp::OpenFile` resolves itself against CONTENT then GAME. Three helpers keep that
//! one call free of all three of its failure modes (idabins
//! docs/flows/file-edit-reload-compile.md §B):
//!
//! * [`open_path_ok`] refuses every path shape that could make OpenFile pop a modal box
//!   ("Cannot open ...", "outside the content dir") and block the tools main thread until a
//!   person clicks it. Only a CANONICAL file under the addon's `maps/` is accepted (review M1:
//!   Win32 opens `maps//x.vmap`, `maps/./x.vmap`, `maps/sub./x.vmap` and `maps/sub /x.vmap` for
//!   the same file, so a second spelling would miss the session-leak gate and leak a session;
//!   the gate refuses them instead of normalising, so only one spelling reaches OpenFile).
//! * [`vmap_header_ok`] refuses a file that does not carry the DMX header the crate's own reader
//!   accepts for binary v9 (review H1: OpenFileInternal pops a modal "Error opening file"
//!   QMessageBox on a load failure, which wedges the drain - the bridge is the second line of
//!   defence, the Rust tool parses the whole file with `hammer-mcp/src/vmap` before it sends
//!   anything).
//! * [`path_eq`] decides whether a session the app already holds IS the file being asked for.
//!   OpenFileInternal's own session-match slot (35, +0x118) is the `xor al,al; ret` stub
//!   `hammer!0x18009DBC0` in this build, so it never matches ANY session and calling OpenFile
//!   for an already-open file creates a SECOND, independent session for the same file (the
//!   first one is leaked and keeps its own undo history and view state). map.open walks the
//!   session vector (`S2_OFF_CBaseEditorApp_m_Sessions`: count, data pointer at +8) itself
//!   first and answers `existing:true` instead of calling OpenFile.
//!
//! All three are pure: no Windows API, no game objects, no libc. The C host unit test
//! (`test_map_path.c`, wired into `build.sh:48-49`) covers the C text; the Rust
//! twin `crates/cs2-bridge/tests/map_path.rs` ports it case for case and adds a differential
//! corpus the compiled C header produces.
//!
//! # The bytes contract
//!
//! Every function takes `&[u8]`: the C works on `char` bytes (no UTF-8 validation anywhere in
//! the header - `map_lc` folds ASCII case only and a UTF-8 byte passes through). Where the C
//! reads a NUL-terminated string, the Rust port scans to the first NUL in the slice (or the end
//! of the slice), so a slice carrying a trailing NUL answers exactly what the C string would.
//!
//! The reason strings are the protocol: `map.open` turns a refusal into
//! `-32602 InvalidParams {reason}`, and the Rust tool's own local check mirrors the same
//! sentences, so [`Reason::as_str`] returns exactly the C literals.

use core::fmt;

/// The bound `map_open_path_ok` puts on the addon-relative path: 512, the same bound
/// `modeldoc.open`/`pet.open` put on theirs (the Rust tools refuse a longer one before
/// dispatch). `MAP_PATH_MAX` in `map_path.h:41`.
pub const MAP_PATH_MAX: usize = 512;

/// How many leading bytes of a `.vmap` [`vmap_header_ok`] needs: the header is
/// `<!-- dmx encoding binary 9 format <fmt> <ver> -->` plus the NUL that ends it, and the
/// `<fmt>`/`<ver>` tail is not fixed, so the caller hands over a window and the helper looks
/// for the NUL inside it. 512 is far more than any DMX header (live: 43 bytes for a vmap) and a
/// caller can always grow it. `MAP_VMAP_HEADER_MAX` in `map_path.h:48`.
pub const MAP_VMAP_HEADER_MAX: usize = 512;

/// 1 when `c` is an ASCII separator in either spelling (`map_path.h:51`).
fn sep(c: u8) -> bool {
    c == b'\\' || c == b'/'
}

/// ASCII whitespace: the byte-level twin of the reader's `split_whitespace` for a DMX header
/// (`map_path.h:56`). Rust's `char::is_whitespace` also accepts the Unicode spaces and
/// the reader accepts 0x85/0xA0; no DMX writer emits one inside the header, and a refusal there
/// is the safe direction.
fn hdr_ws(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0x0b || c == 0x0c
}

/// Lowercase an ASCII byte (only ASCII is folded: a UTF-8 byte passes through)
/// (`map_path.h:61`).
fn lc(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b | 0x20
    } else {
        b
    }
}

/// Why [`open_path_ok`] or [`vmap_header_ok`] refused the input.
///
/// Every variant is one of the C header's static reasons, spelled the same way by
/// [`Reason::as_str`] because the caller puts that string into the protocol's
/// `-32602 InvalidParams {reason}` (`map_vmap_header_ok` refusals go out as `InvalidMapFile`
/// and are only logged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The path is empty (`map_path.h:83`).
    Empty,
    /// The path is over [`MAP_PATH_MAX`] bytes (`map_path.h:88`).
    TooLong,
    /// The path starts with a separator or a drive letter (`map_path.h:92`).
    Absolute,
    /// A byte below 0x20 or 0x7F in the path (`map_path.h:98`).
    Control,
    /// A `:` in the path: what it would name on Windows is an alternate data stream, never a
    /// map (`map_path.h:104`).
    Colon,
    /// A `\` in the path: the canonical spelling uses `/` only (review M1,
    /// `map_path.h:111`).
    Backslash,
    /// The path does not end in `.vmap` (case-insensitive) (`map_path.h:117`).
    NotVmap,
    /// The path is not under `maps/` (case-insensitive) (`map_path.h:122`).
    NotUnderMaps,
    /// `maps/.vmap`: a file directly under `maps/` with no basename (`map_path.h:127`).
    NoBasename,
    /// Two consecutive dots anywhere in the path - the same `..` rule `modeldoc.open`/
    /// `pet.open` use (`map_path.h:132`).
    DotDot,
    /// An empty path segment: `//` (review M1, `map_path.h:144`).
    EmptySegment,
    /// A `.` or `..` segment (review M1, `map_path.h:148`).
    DotSegment,
    /// A segment ending in `.` or a space (review M1, `map_path.h:152`).
    TrailingDotSpace,
    /// The header window is empty (`map_path.h:181`).
    EmptyFile,
    /// The window holds no NUL, so there is no NUL-terminated header inside it
    /// (`map_path.h:186`).
    NoNul,
    /// The header does not start with `<!-- dmx ` (`map_path.h:191`).
    NotDmx,
    /// The header's first word is not `encoding` (`map_path.h:205`).
    FirstWord,
    /// The header's second word is not `binary` (`map_path.h:212`).
    NotBinary,
    /// The header's third word does not parse as a number (`map_path.h:219`/`:224`).
    VersionNotNumber,
    /// The header's encoding version is not 9 (`map_path.h:231`).
    VersionNot9,
    /// Fewer than six words in the header (`map_path.h:237`).
    TooFewWords,
}

impl Reason {
    /// The reason exactly as the C header reports it (`map_path.h`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::Empty => "path: a non-empty path relative to the addon content root \
                              (maps/... .vmap) is required",
            Reason::TooLong => "path: 1..512 chars",
            Reason::Absolute => "path: relative to the addon content root (maps/...), not absolute",
            Reason::Control => "path: no control characters",
            Reason::Colon => "path: no ':'",
            Reason::Backslash => "path: no '\\' (use '/' separators)",
            Reason::NotVmap => "path: a .vmap file",
            Reason::NotUnderMaps => "path: under maps/ (the addon's map directory)",
            Reason::NoBasename => "path: a map file name under maps/",
            Reason::DotDot => "path: no '..'",
            Reason::EmptySegment => "path: no empty path segment ('//')",
            Reason::DotSegment => "path: no '.' or '..' path segment",
            Reason::TrailingDotSpace => "path: no path segment ending in '.' or a space",
            Reason::EmptyFile => "empty file",
            Reason::NoNul => "no NUL-terminated DMX header in the first bytes",
            Reason::NotDmx => "not a DMX file (the header must start with '<!-- dmx ')",
            Reason::FirstWord => "the DMX header's first word is not 'encoding'",
            Reason::NotBinary => "the DMX encoding is not 'binary'",
            Reason::VersionNotNumber => "the DMX binary encoding version is not a number",
            Reason::VersionNot9 => "the DMX binary encoding version is not 9",
            Reason::TooFewWords => {
                "the DMX header is too short (encoding, binary, 9, format, <fmt>, <ver>)"
            }
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 1 when `path` is a usable addon-content-relative map path, 0 = `*reason` names the gate that
/// refused it (`map_path.h:79`). Ported statement for statement; every gate below cites
/// the C line it mirrors.
pub fn open_path_ok(path: &[u8]) -> Result<(), Reason> {
    // `if (!path || !path[0])` (map_path.h:82): a null pointer cannot reach Rust; the empty
    // string is the slice that scans to 0 bytes.
    let n = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    if n == 0 {
        return Err(Reason::Empty);
    }
    // `if (n > MAP_PATH_MAX)` (map_path.h:87).
    if n > MAP_PATH_MAX {
        return Err(Reason::TooLong);
    }
    // `if (map_sep(path[0]) || (n > 1 && path[1] == ':'))` (map_path.h:91).
    if sep(path[0]) || (n > 1 && path[1] == b':') {
        return Err(Reason::Absolute);
    }
    // the per-byte scan (map_path.h:95-114).
    for &c in &path[..n] {
        if c < 0x20 || c == 0x7f {
            return Err(Reason::Control);
        }
        if c == b':' {
            return Err(Reason::Colon);
        }
        if c == b'\\' {
            return Err(Reason::Backslash);
        }
    }
    // the `.vmap` suffix (map_path.h:115-119).
    if n < 6
        || path[n - 5] != b'.'
        || lc(path[n - 4]) != b'v'
        || lc(path[n - 3]) != b'm'
        || lc(path[n - 2]) != b'a'
        || lc(path[n - 1]) != b'p'
    {
        return Err(Reason::NotVmap);
    }
    // the `maps/` prefix (map_path.h:120-124).
    if !(lc(path[0]) == b'm'
        && lc(path[1]) == b'a'
        && lc(path[2]) == b'p'
        && lc(path[3]) == b's'
        && path[4] == b'/')
    {
        return Err(Reason::NotUnderMaps);
    }
    let base = 5; // the first byte after "maps/" (map_path.h:125)
    // `if (n <= base + 5)` (map_path.h:126): "maps/" + ".vmap", a name with no basename.
    if n <= base + 5 {
        return Err(Reason::NoBasename);
    }
    // the `..` scan (map_path.h:130-135).
    for i in 0..n - 1 {
        if path[i] == b'.' && path[i + 1] == b'.' {
            return Err(Reason::DotDot);
        }
    }
    // the canonical-segment scan (map_path.h:139-157). A segment is [start, i) with i at the
    // terminating '/' or at n; the last one cannot be empty (the ".vmap" suffix check above),
    // so an empty segment is always a `//`.
    let mut start = base;
    for i in base..=n {
        if i != n && path[i] != b'/' {
            continue;
        }
        if i == start {
            return Err(Reason::EmptySegment);
        }
        if i - start == 1 && path[start] == b'.' {
            return Err(Reason::DotSegment);
        }
        if path[i - 1] == b'.' || path[i - 1] == b' ' {
            return Err(Reason::TrailingDotSpace);
        }
        start = i + 1; // the next segment starts after the '/'
    }
    Ok(())
}

/// 1 when `head` (a window into the first bytes of a `.vmap`) carries a DMX header binary v9
/// accepts, 0 = `*reason` names why not (`map_path.h:177`).
///
/// This mirrors the reader's own precondition (`hammer-mcp/src/vmap/dmx.rs`, the retired
/// oracle `vmap_dmx.py`): the file starts with the bytes of the header up to a NUL,
/// the header starts with `<!-- dmx `, its first three whitespace-separated words are
/// `encoding`, `binary` and the encoding version, and there are at least six words (encoding,
/// binary, 9, format, <fmt>, <ver>). The first 16 bytes are NOT enough to tell a `binary 9`
/// from a `binary 8` or a `keyvalues2 1` (`<!-- dmx encodin` is shared by all three spellings),
/// which is why this compares the words. The reader also accepts `parts[3]` being any word (it
/// never checks one), and a version word with leading zeros or a sign; this accepts those too
/// (both parse to the same i32), so nothing the reader would accept is refused here.
pub fn vmap_header_ok(head: &[u8]) -> Result<(), Reason> {
    // `if (!head || !len)` (map_path.h:180): a null pointer cannot reach Rust.
    let len = head.len();
    if len == 0 {
        return Err(Reason::EmptyFile);
    }
    // the header string ends at its NUL (map_path.h:184).
    let mut n = 0;
    while n < len && head[n] != 0 {
        n += 1;
    }
    // `if (n == len)` (map_path.h:185): no NUL inside the window.
    if n == len {
        return Err(Reason::NoNul);
    }
    // the `<!-- dmx ` prefix (map_path.h:189-193).
    if n < 8 || head[..8] != *b"<!-- dmx" {
        return Err(Reason::NotDmx);
    }
    // the word walk (map_path.h:194-235).
    let mut words = 0;
    let mut i = 8;
    while i < n {
        while i < n && hdr_ws(head[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }
        let start = i;
        while i < n && !hdr_ws(head[i]) {
            i += 1;
        }
        words += 1;
        if words == 1 {
            // `encoding` (map_path.h:201-207).
            if i - start != 8 || head[start..start + 8] != *b"encoding" {
                return Err(Reason::FirstWord);
            }
        } else if words == 2 {
            // `binary` (map_path.h:208-214).
            if i - start != 6 || head[start..start + 6] != *b"binary" {
                return Err(Reason::NotBinary);
            }
        } else if words == 3 {
            // the encoding version (map_path.h:215-234): an optional sign, digits only, and the
            // value must be 9 (a leading `-` refuses even `-9`: the reader cannot parse a
            // negative into the i32 it compares against 9).
            let mut k = start;
            let mut v: u32 = 0;
            if k < i && (head[k] == b'+' || head[k] == b'-') {
                k += 1;
            }
            if k >= i {
                // "" or a lone sign: the reader cannot parse it
                return Err(Reason::VersionNotNumber);
            }
            while k < i {
                if !head[k].is_ascii_digit() {
                    return Err(Reason::VersionNotNumber);
                }
                v = v * 10 + u32::from(head[k] - b'0');
                if v > 9999 {
                    v = 9999; // saturate: any other value is "not 9" anyway
                }
                k += 1;
            }
            if head[start] == b'-' || v != 9 {
                return Err(Reason::VersionNot9);
            }
        }
    }
    // `if (words < 6)` (map_path.h:236-239).
    if words < 6 {
        return Err(Reason::TooFewWords);
    }
    Ok(())
}

/// Byte equality of `n` bytes, folding ASCII case and both separator spellings
/// (`map_path.h:244`).
fn path_eq_n(a: &[u8], b: &[u8], n: usize) -> bool {
    for i in 0..n {
        let mut ca = lc(a[i]);
        let mut cb = lc(b[i]);
        if sep(ca) {
            ca = b'/';
        }
        if sep(cb) {
            cb = b'/';
        }
        if ca != cb {
            return false;
        }
    }
    true
}

/// 1 when the two paths name the same file. Separators (`\` and `/`) and ASCII case are folded
/// and a trailing separator is dropped; after that the two must be byte-equal
/// (`map_path.h:267`).
///
/// REVIEW L2: there is no relative-tail rule. OpenFileInternal always stores a full path (it
/// uses either `V_IsAbsolutePath` or the CONTENT/GAME `RelativePathToFullPath`, and returns 0
/// when neither resolves), and the resolved host path map.open compares against is always
/// absolute (`editor_app_root` + `kv3_host_path`), so a tail match can never be the right
/// answer - it could only claim a DIFFERENT file (another addon's `maps/x.vmap`). Two paths
/// that are not equal after folding are two files. Non-ASCII case and 8.3 short names are not
/// folded (also Low in the review); neither is a component's trailing `.`/space, which cannot
/// occur on either side ([`open_path_ok`] refuses it in the requested path, and Hammer stores
/// what its resolver found).
pub fn path_eq(a: &[u8], b: &[u8]) -> bool {
    // The C reads NUL-terminated strings (`while (a[la]) la++`, map_path.h:270-271); a slice
    // ends at its first NUL.
    let a_len = a.iter().position(|&c| c == 0).unwrap_or(a.len());
    let b_len = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    // `if (!a || !b) return 0` (map_path.h:269): a null pointer cannot reach Rust.
    // A trailing separator is not part of the name (map_path.h:272-273).
    let mut la = a_len;
    let mut lb = b_len;
    while la > 0 && sep(a[la - 1]) {
        la -= 1;
    }
    while lb > 0 && sep(b[lb - 1]) {
        lb -= 1;
    }
    // `if (!la || !lb) return 0` (map_path.h:274).
    if la == 0 || lb == 0 {
        return false;
    }
    la == lb && path_eq_n(a, b, la)
}
