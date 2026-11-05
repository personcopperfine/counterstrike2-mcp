//! `VPK` reader tests (WP-V): the synthetic fixtures are built as bytes in these tests (v1 +
//! v2, root dir `" "`, no-extension ext, preload-only entries, `0x7FFF` in-dir data, sibling
//! archive data, and — by patching the crafted bytes — every malformed corner the reader
//! promises to refuse). Nothing here touches /tmp: fixtures live under `CARGO_TARGET_TMPDIR`
//! and are removed on drop.
//!
//! The `#[ignore]`d tests at the bottom open the real install's `game/csgo/pak01_dir.vpk` and
//! `game/core/pak01_dir.vpk` (read-only, never copied into the repo):
//! `cargo test -p hammer-mcp --test vpk -- --ignored`.
#![allow(clippy::result_large_err)]

mod common;

use common::{write_vpk, BuildFile};
use hammer_mcp::vpk::{VpkDir, VpkError, MAX_READ_BYTES};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh temp dir under `CARGO_TARGET_TMPDIR`, removed on drop.
struct Ws {
    root: PathBuf,
}
impl Ws {
    fn new(tag: &str) -> Ws {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "vpk-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("ws");
        Ws { root }
    }
}
impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A valid directory file's raw shape for one file (the smallest tree that parses): ext
/// `vmdl_c`, dir `models`, one entry. Returns the tree and the patch positions a malformed
/// case needs (all offsets are tree-relative; `assemble` writes the 12/28-byte header first).
struct Sketch {
    tree: Vec<u8>,
    /// The entry's terminator position inside the tree (2 bytes).
    terminator_at: usize,
    /// The entry's preload-length position inside the tree (2 bytes).
    preload_len_at: usize,
    /// The entry's `entry_offset` position inside the tree (4 bytes).
    offset_at: usize,
    /// The entry's `entry_length` position inside the tree (4 bytes).
    length_at: usize,
}

/// ext `ext`, dir `dir`, one entry naming `name` with `preload` bytes, archive index
/// `archive` claiming `length` bytes (with `data` written behind the tree when it is in-dir;
/// otherwise write the sibling yourself). The reader does not care about crc (`0`).
fn sketch(name: &str, ext: &str, dir: &str, preload: &[u8], archive: u16, length: u32) -> Sketch {
    let mut tree = Vec::new();
    tree.extend_from_slice(ext.as_bytes());
    tree.push(0);
    tree.extend_from_slice(dir.as_bytes());
    tree.push(0);
    tree.extend_from_slice(name.as_bytes());
    tree.push(0);
    tree.extend_from_slice(&0u32.to_le_bytes()); // crc
    let preload_len_at = tree.len();
    tree.extend_from_slice(&(preload.len() as u16).to_le_bytes());
    tree.extend_from_slice(&archive.to_le_bytes());
    let offset_at = tree.len();
    tree.extend_from_slice(&0u32.to_le_bytes());
    let length_at = tree.len();
    tree.extend_from_slice(&length.to_le_bytes());
    let terminator_at = tree.len();
    tree.extend_from_slice(&0xFFFFu16.to_le_bytes());
    tree.extend_from_slice(preload);
    tree.push(0); // dir group terminator
    tree.push(0); // ext group terminator
    tree.push(0); // the ext list's own terminator
    Sketch {
        tree,
        terminator_at,
        preload_len_at,
        offset_at,
        length_at,
    }
}

/// One tree (`data` lands behind it as in-dir data) -> the raw `_dir.vpk` bytes.
fn assemble(version: u32, tree: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x55AA_1234u32.to_le_bytes());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(tree.len() as u32).to_le_bytes());
    if version == 2 {
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    out.extend_from_slice(tree);
    out.extend_from_slice(data);
    out
}

/// Write the bytes as `<ws>/x_dir.vpk` and open it.
fn open_raw(ws: &Ws, bytes: &[u8]) -> Result<VpkDir, VpkError> {
    let path = ws.root.join("x_dir.vpk");
    std::fs::write(&path, bytes).expect("patched dir");
    VpkDir::open(&path)
}

// -------------------------------------------------------------------------------------------
// synthetic round-trips
// -------------------------------------------------------------------------------------------

/// A v1 VPK: a root-dir file with no extension, a normal subdirectory model, a preload-only
/// entry (nothing to fetch from any archive); sorted lowercase paths; `contains`
/// case-insensitive with backslashes.
#[test]
fn v1_directory_parses_root_files_and_preload_only() {
    let ws = Ws::new("v1");
    let built = write_vpk(
        &ws.root,
        "pak01",
        1,
        &[
            BuildFile::loaded(
                "models/props/box.vmdl_c",
                b"box-data",
                b"box-preload",
                false,
            ),
            BuildFile::preload_only("materials/brief.vmat", b"brief-preload"),
            BuildFile::preload_only("test", b""), // root dir, no extension, empty preload
        ],
    );
    let vpk = VpkDir::open(&built.dir).expect("v1 dir");

    assert_eq!(vpk.len(), 3, "three files");
    assert!(!vpk.is_empty());
    let paths: Vec<&str> = vpk.paths().collect();
    assert_eq!(
        paths,
        vec!["materials/brief.vmat", "models/props/box.vmdl_c", "test"],
        "sorted lowercase paths"
    );
    assert!(
        vpk.contains("MODELS\\PROPS\\BOX.VMDL_C"),
        "case + backslash"
    );
    assert!(vpk.contains("materials/brief.vmat"));
    assert!(
        !vpk.contains("models/props/box.vmdl"),
        "extensionless lookup"
    );

    assert_eq!(
        vpk.read("models/props/box.vmdl_c").unwrap(),
        b"box-preloadbox-data".to_vec(),
        "read = preload ++ sibling data"
    );
    assert_eq!(vpk.read("materials/brief.vmat").unwrap(), b"brief-preload");
    assert_eq!(vpk.read("test").unwrap(), b"", "empty preload-only read");
}

/// A v2 VPK: some data lives in-dir (`0x7FFF` — read from the directory file behind the
/// tree), some in the `pak01_000.vpk` sibling (seek + bounded read).
#[test]
fn v2_directory_reads_in_dir_and_sibling_data() {
    let ws = Ws::new("v2");
    let built = write_vpk(
        &ws.root,
        "pak01",
        2,
        &[
            BuildFile::loaded("models/dust/wall.vmdl_c", b"wall-data!", b"wall-pre", true),
            BuildFile::loaded("models/dust/pipes.vmdl_c", b"pipe-data", b"", false),
        ],
    );
    assert_eq!(built.archives.len(), 1, "the sibling was written");
    assert!(built.archives[0].ends_with("pak01_000.vpk"));

    let vpk = VpkDir::open(&built.dir).expect("v2 dir");
    assert_eq!(
        vpk.read("models/dust/wall.vmdl_c").unwrap(),
        b"wall-prewall-data!".to_vec(),
        "in-dir: preload + data out of the directory file"
    );
    assert_eq!(
        vpk.read("models/dust/pipes.vmdl_c").unwrap(),
        b"pipe-data".to_vec(),
        "sibling: data out of pak01_000.vpk"
    );
}

/// An unknown path is `NotFound` with the asked spelling; an empty tree opens with len 0.
#[test]
fn not_found_and_the_empty_tree() {
    let ws = Ws::new("nf");
    let built = write_vpk(&ws.root, "pak01", 2, &[]);
    let vpk = VpkDir::open(&built.dir).expect("the empty tree parses");
    assert!(vpk.is_empty());
    let error = vpk.read("models/props/box.vmdl_c").unwrap_err();
    assert!(matches!(error, VpkError::NotFound(_)), "{error}");
    assert!(
        error.to_string().contains("models/props/box.vmdl_c"),
        "{error}"
    );
}

// -------------------------------------------------------------------------------------------
// malformed inputs (patched bytes; the reader answers with an error, never a panic)
// -------------------------------------------------------------------------------------------

/// Bad magic / unsupported version are the opening refusals, both naming what was found.
#[test]
fn bad_magic_and_version_are_format_errors() {
    let ws = Ws::new("magic");
    let sketch = sketch("box", "vmdl_c", "models", b"prel", 0, 4);
    let bytes = assemble(2, &sketch.tree, b"data");

    let mut wrong_magic = bytes.clone();
    wrong_magic[0] ^= 0xFF;
    let error = open_raw(&ws, &wrong_magic).unwrap_err();
    assert!(error.to_string().contains("bad magic"), "{error}");

    let mut wrong_version = bytes;
    wrong_version[4..8].copy_from_slice(&3u32.to_le_bytes());
    let error = open_raw(&ws, &wrong_version).unwrap_err();
    assert!(error.to_string().contains("version 3"), "{error}");
}

/// `tree_size` larger than the file: there cannot be a tree there.
#[test]
fn tree_size_past_the_file_is_a_format_error() {
    let ws = Ws::new("bigtree");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0, 4);
    let mut bytes = assemble(2, &sketch.tree, b"data");
    let inflated = (u32::from_le_bytes(bytes[8..12].try_into().unwrap()) + 4096).to_le_bytes();
    bytes[8..12].copy_from_slice(&inflated);
    let error = open_raw(&ws, &bytes).unwrap_err();
    assert!(error.to_string().contains("tree_size"), "{error}");
}

/// A name without its `\0` anywhere behind it: the scan runs off the tree's end.
#[test]
fn an_unterminated_name_is_a_format_error() {
    let ws = Ws::new("unterminated");
    // ext / dir / unterminated name, then entry bytes spelled so that NO zero byte follows
    // the name (otherwise the scan would stop at the first one and mis-parse instead of
    // reporting the truncated tree): crc/preload/archive/offset/length 0x11..1s, terminator
    // 0xFFFF, preload 'x'.
    let mut tree = b"vmdl_c\0models\0box".to_vec();
    tree.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // crc
    tree.extend_from_slice(&0x1111u16.to_le_bytes()); // preload
    tree.extend_from_slice(&0x1111u16.to_le_bytes()); // archive
    tree.extend_from_slice(&0x1111_1111u32.to_le_bytes()); // offset
    tree.extend_from_slice(&0x1111_1111u32.to_le_bytes()); // length
    tree.extend_from_slice(&0xFFFFu16.to_le_bytes());
    tree.push(b'x'); // the (length-1) preload byte, with nothing beyond either
    let bytes = assemble(1, &tree, b"");
    let error = open_raw(&ws, &bytes).unwrap_err();
    assert!(error.to_string().contains("unterminated"), "{error}");
}

/// An entry whose terminator is not `0xFFFF`.
#[test]
fn a_bad_entry_terminator_is_a_format_error() {
    let ws = Ws::new("terminator");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0, 4);
    let mut bytes = assemble(2, &sketch.tree, b"data");
    let at = 28 + sketch.terminator_at;
    bytes[at..at + 2].copy_from_slice(&0xFFFEu16.to_le_bytes());
    let error = open_raw(&ws, &bytes).unwrap_err();
    assert!(error.to_string().contains("terminator"), "{error}");
}

/// A preload running past the tree's end (its bytes cannot be there).
#[test]
fn preload_past_the_tree_is_a_format_error() {
    let ws = Ws::new("preload");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0, 4);
    let mut bytes = assemble(2, &sketch.tree, b"data");
    let at = 28 + sketch.preload_len_at;
    bytes[at..at + 2].copy_from_slice(&60_000u16.to_le_bytes());
    let error = open_raw(&ws, &bytes).unwrap_err();
    assert!(error.to_string().contains("preload"), "{error}");
}

/// In-dir data beyond the directory file: refused at open, before any read.
#[test]
fn in_dir_data_past_the_file_is_a_format_error_at_open() {
    let ws = Ws::new("indir-past");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0x7FFF, 4);
    let mut bytes = assemble(2, &sketch.tree, b"data");
    let at = 28 + sketch.offset_at;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let error = open_raw(&ws, &bytes).unwrap_err();
    assert!(error.to_string().contains("in-dir data"), "{error}");
}

/// A tree that ends before `tree_size` says — trailing bytes inside the declared tree.
#[test]
fn trailing_tree_bytes_are_a_format_error() {
    let ws = Ws::new("trailing");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0, 4);
    let mut tree = sketch.tree.clone();
    tree.push(0x01); // junk inside the still-declared tree region
    let bytes = assemble(1, &tree, b"data");
    let error = open_raw(&ws, &bytes).unwrap_err();
    assert!(error.to_string().contains("trailing"), "{error}");
}

/// The same full path listed twice is refused, never silently overwritten.
#[test]
fn a_duplicated_tree_path_is_a_format_error() {
    let ws = Ws::new("duplicate");
    let mut tree = Vec::new();
    for _ in 0..2 {
        // the same ext/dir/name group twice: the second insert is the duplicate
        tree.extend_from_slice(b"vmdl_c\0models\0box\0");
        tree.extend_from_slice(&0u32.to_le_bytes()); // crc
        tree.extend_from_slice(&0u16.to_le_bytes()); // preload 0
        tree.extend_from_slice(&0u16.to_le_bytes()); // archive (0, no in-dir marker)
        tree.extend_from_slice(&0u32.to_le_bytes()); // offset
        tree.extend_from_slice(&0u32.to_le_bytes()); // length
        tree.extend_from_slice(&0xFFFFu16.to_le_bytes());
        tree.push(0); // dir group terminator
        tree.push(0); // ext group terminator
    }
    tree.push(0); // the ext list's own terminator
    let bytes = assemble(1, &tree, b"");
    let error = open_raw(&ws, &bytes).unwrap_err();
    assert!(error.to_string().contains("duplicate"), "{error}");
}

/// The read cap: an `entry_length` patch of just over 256 MiB is answered `TooLarge` before
/// any allocation (the tree itself still parses).
#[test]
fn a_read_past_the_cap_is_too_large() {
    let ws = Ws::new("toolarge");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0, 4);
    let mut bytes = assemble(2, &sketch.tree, b"data");
    let at = 28 + sketch.length_at;
    bytes[at..at + 4].copy_from_slice(&((MAX_READ_BYTES + 1) as u32).to_le_bytes());
    let dir = ws.root.join("pak01_dir.vpk");
    std::fs::write(&dir, &bytes).expect("dir file");
    let vpk = VpkDir::open(&dir).expect("a huge length alone does not corrupt the tree");
    let error = vpk.read("models/box.vmdl_c").unwrap_err();
    assert!(matches!(error, VpkError::TooLarge { .. }), "{error}");
    match error {
        VpkError::TooLarge { path, size, max } => {
            assert_eq!(path, "models/box.vmdl_c");
            assert_eq!(size, MAX_READ_BYTES + 2, "preload(1) + length");
            assert_eq!(max, MAX_READ_BYTES);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

/// The sibling archive is smaller than the entry claims: `read` names the sibling.
#[test]
fn sibling_data_past_the_siblings_eof_is_a_format_error() {
    let ws = Ws::new("sib-eof");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0, 1024);
    let mut bytes = assemble(2, &sketch.tree, b"data");
    let at = 28 + sketch.length_at;
    bytes[at..at + 4].copy_from_slice(&1024u32.to_le_bytes()); // only 4 bytes exist
    std::fs::write(ws.root.join("pak01_dir.vpk"), &bytes).expect("dir file");
    std::fs::write(ws.root.join("pak01_000.vpk"), b"only").expect("short sibling");
    let vpk = VpkDir::open(&ws.root.join("pak01_dir.vpk")).expect("parses");
    let error = vpk.read("models/box.vmdl_c").unwrap_err();
    assert!(
        error.to_string().contains("pak01_000.vpk"),
        "the sibling is named: {error}"
    );
    assert!(matches!(error, VpkError::Format(_)), "{error}");
}

/// The real install's preload-only shape: `archive_index == 0x7FFF` with the sentinel offset
/// `0xFFFFFFFF` and length 0 (the core pak's two `0x7FFF` entries) — the open must not run
/// the in-dir bounds check on a zero length, and the read answers the preload bytes only.
#[test]
fn the_sentinel_shaped_preload_only_entry_parses_and_reads() {
    let ws = Ws::new("sentinel");
    let sketch = sketch("window_keybinds", "cfg", "panorama", b"keybits", 0x7FFF, 0);
    let mut bytes = assemble(2, &sketch.tree, b"");
    let at = 28 + sketch.offset_at;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let dir = ws.root.join("pak01_dir.vpk");
    std::fs::write(&dir, &bytes).expect("dir file");
    let vpk = VpkDir::open(&dir).expect("the real preload-only shape parses");
    assert_eq!(
        vpk.read("panorama/window_keybinds.cfg").unwrap(),
        b"keybits".to_vec(),
        "only the preload bytes"
    );
}

/// The sibling archive that does not exist: an io error naming the expected file.
#[test]
fn a_missing_sibling_is_an_io_error() {
    let ws = Ws::new("sib-missing");
    let sketch = sketch("box", "vmdl_c", "models", b"x", 0, 4);
    let bytes = assemble(2, &sketch.tree, b"data");
    std::fs::write(ws.root.join("pak01_dir.vpk"), &bytes).expect("dir file");
    let vpk = VpkDir::open(&ws.root.join("pak01_dir.vpk")).expect("parses");
    let error = vpk.read("models/box.vmdl_c").unwrap_err();
    assert!(matches!(error, VpkError::Io { .. }), "{error}");
    assert!(error.to_string().contains("pak01_000.vpk"), "{error}");
}

// -------------------------------------------------------------------------------------------
// the real install (#[ignore]; run with `cargo test -p hammer-mcp --test vpk -- --ignored`)
// -------------------------------------------------------------------------------------------

/// The project root (the `tests/entities_file.rs` rule).
fn project_root() -> PathBuf {
    const MARKER: &str = "workshop/run/app/game/csgo/csgo.fgd";
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("CS2_MCP_PROJECT_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        candidates.push(dir.clone());
        if !dir.pop() {
            break;
        }
    }
    candidates
        .into_iter()
        .find(|dir| dir.join(MARKER).is_file())
        .unwrap_or_else(|| {
            panic!(
                "no project root with {MARKER} at $CS2_MCP_PROJECT_ROOT or above {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

/// The WP's corpus check on the real csgo pak: the directory opens, `models/*.vmdl_c` counts
/// over 1000, and one read model starts like a Source 2 resource (u32 file size == data len,
/// u16 header version 12).
#[tokio::test]
#[ignore]
async fn real_install_csgo_pak_model_counts_and_resource_shape() {
    let dir = project_root().join("workshop/run/app/game/csgo/pak01_dir.vpk");
    let vpk = VpkDir::open(&dir).expect("the real csgo pak01_dir.vpk");
    let models: Vec<&str> = vpk
        .paths()
        .filter(|path| path.starts_with("models/") && path.ends_with(".vmdl_c"))
        .collect();
    assert!(
        models.len() > 1000,
        "the WP promised over 1000 models/*.vmdl_c, got {}",
        models.len()
    );
    let picked = models
        .iter()
        .find(|path| path.contains("props"))
        .or(models.first())
        .copied()
        .expect("models non-empty");
    let bytes = vpk.read(picked).expect("the read lands");
    let file_size = u32::from_le_bytes(bytes[0..4].try_into().expect("resource header"));
    let header_version = u16::from_le_bytes(bytes[4..6].try_into().expect("resource header"));
    assert_eq!(
        usize::try_from(file_size).unwrap(),
        bytes.len(),
        "u32 file size == data len"
    );
    assert_eq!(header_version, 12, "u16 header version");
    println!(
        "{picked}: {} bytes, size field {file_size}, header version {header_version}",
        bytes.len()
    );
}

/// The second required pak (core) opens too; the csgo tree holds its real 137k entries.
#[tokio::test]
#[ignore]
async fn real_install_core_pak_opens() {
    let csgo = VpkDir::open(&project_root().join("workshop/run/app/game/csgo/pak01_dir.vpk"))
        .expect("csgo");
    assert!(csgo.len() > 100_000, "csgo holds {}", csgo.len());
    let core = VpkDir::open(&project_root().join("workshop/run/app/game/core/pak01_dir.vpk"))
        .expect("core");
    assert!(!core.is_empty(), "core holds {}", core.len());
}
