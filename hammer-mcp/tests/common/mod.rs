//! Hermetic defaults shared by the test crates. Without them a config resolves the host's project
//! tree (DEFAULT_PROJECT_ROOT is baked from CARGO_MANIFEST_DIR: its console.log, compatdata, artifacts
//! and overviews), the real podman (running cs2mcp-* containers), the built capture layer under
//! target/, and the newest /run/user/<uid>/xauth_* cookie. The project, podman and layer defaults
//! point at paths that do not exist, so an unset flag means "nothing there". The X cookie default is
//! a file under this crate's CARGO_TARGET_TMPDIR, so a wine-tools launch finds a cookie without
//! reading the host's. A test that needs another value passes its own, which wins (see [`hermetic`]).
#![allow(dead_code)]
#![allow(clippy::result_large_err)]

pub mod gltf_fixture;
pub mod humanoid;
pub mod agent_data;
pub mod vnmskel_data;

use hammer_mcp::resource::kv3bin::Kv3Value;

const XAUTHORITY: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/hermetic-xauth");

const HERMETIC: &[(&str, &str)] = &[
    ("--project-root", "/nonexistent/cs2mcp-test-project"),
    ("--podman", "/nonexistent/cs2mcp-test-podman"),
    ("--vk-layer-dir", "/nonexistent/cs2mcp-test-vklayer"),
    ("--xauthority", XAUTHORITY),
];

/// `args` with each hermetic default prepended, unless `args` already passes that flag (by name or
/// as `--flag=value`). Clap rejects a repeated single-value flag, so the override has to be exact.
pub fn hermetic<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(args.len() + 2 * HERMETIC.len());
    for (flag, value) in HERMETIC {
        let given = args
            .iter()
            .any(|a| *a == *flag || a.starts_with(&format!("{flag}=")));
        if !given {
            if *flag == "--xauthority" && !std::path::Path::new(XAUTHORITY).exists() {
                std::fs::write(XAUTHORITY, "cookie").expect("hermetic cookie file");
            }
            out.push(*flag);
            out.push(*value);
        }
    }
    out.extend_from_slice(args);
    out
}

// -------------------------------------------------------------------------------------------
// FGD fixture tree (WP-R2: tests/node_transform.rs + tests/entity_io.rs). Copies the committed
// mini FGD fixture (`hammer-mcp/fixtures/fgd`) into a temp project tree and appends the prop
// classes those two suites need (`props_mini.fgd`, `@include`d by the copied `csgo/csgo.fgd`) —
// the shared fixture itself is left untouched.
// -------------------------------------------------------------------------------------------

/// The prop classes appended to the copied FGD (shapes mirror the real `core/base.fgd`):
/// `prop_static` is `tags(PropStatic)` with **no inputs** (it is compiled into the world), and
/// `prop_dynamic` inherits its inputs/outputs from a `tags(PropDynamic)` base class — so the
/// tag/base-chain walk and the input lists both have something real to resolve.
pub const PROPS_FGD: &str = "\
// WP-R2 test fixture: the prop classes the m2.18 transform / entity-I/O suites check against.
@PointClass base(Targetname) tags( PropStatic ) = prop_static : \"A compile-time static prop, baked into the world.\"

@BaseClass base(Targetname) tags( PropDynamic ) = prop_dynamic_base : \"A dynamic prop.\"
[
	input SetAnimation(string) : \"Force the prop to play an animation.\"
	input TurnOn(void) : \"Make the prop visible.\"
	output OnAnimationBegun(void) : \"Fired whenever a new animation has begun playing.\"
]

@PointClass base(prop_dynamic_base) = prop_dynamic : \"Dynamic prop.\"

@PointClass base(Targetname) = trigger_multiple : \"Trigger.\"
[
	input Enable(void) : \"Enable the trigger.\"
	output OnStartTouch(void) : \"Fired when an entity starts touching this trigger.\"
]
";

/// One temp project tree with the FGD fixture in place (`<root>/workshop/run/app/game`, what
/// `fgd::load_for` reads). Removed on drop; never under `/tmp` (`CARGO_TARGET_TMPDIR`).
pub struct FgdTree {
    pub root: std::path::PathBuf,
}

impl FgdTree {
    /// The fixture FGD plus [`PROPS_FGD`].
    pub fn new(tag: &str) -> FgdTree {
        let tree = FgdTree::new_root(tag);
        let fixture = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fgd"));
        copy_tree(fixture, &tree.game());
        assert!(
            tree.game().join("csgo/csgo.fgd").is_file(),
            "the FGD fixture copy is incomplete: {}",
            tree.game().display()
        );
        std::fs::write(tree.game().join("csgo/props_mini.fgd"), PROPS_FGD).expect("props fgd");
        let entry = tree.game().join("csgo/csgo.fgd");
        let mut text = std::fs::read_to_string(&entry).expect("fixture csgo.fgd");
        text.push_str("\n@include \"props_mini.fgd\"\n");
        std::fs::write(&entry, text).expect("csgo.fgd with the props include");
        tree
    }

    /// A tree with no `csgo/csgo.fgd` at all: the "fgd unavailable" warning path.
    pub fn without_fgd(tag: &str) -> FgdTree {
        let tree = FgdTree::new_root(tag);
        assert!(!tree.game().join("csgo/csgo.fgd").exists());
        tree
    }

    fn new_root(tag: &str) -> FgdTree {
        let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "fgd-tree-{tag}-{}-{}",
            std::process::id(),
            TRAIL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("workshop/run/app")).expect("temp tree");
        FgdTree { root: dir }
    }

    /// `<project>/workshop/run/app/game`.
    pub fn game(&self) -> std::path::PathBuf {
        self.root.join("workshop/run/app/game")
    }

    /// `<project>/workshop/run/app`.
    pub fn app(&self) -> std::path::PathBuf {
        self.root.join("workshop/run/app")
    }
}

impl Drop for FgdTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

static TRAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Recursive copy; a missing source fails loudly.
pub fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    let entries = std::fs::read_dir(from)
        .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", from.display()));
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("{}: {e}", to.display()));
    for entry in entries {
        let entry = entry.expect("fixture entry");
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &dest);
        } else {
            std::fs::copy(&path, &dest)
                .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", path.display(), dest.display()));
        }
    }
}

// -------------------------------------------------------------------------------------------
// VPK fixture builder (WP-V: tests/vpk.rs + tests/model_catalog.rs). Builds a real
// (synthetic) VPK pair as bytes: a `_dir.vpk` with the tree (+ in-dir data) and the
// `pak01_000.vpk` sibling for the archive files.
// -------------------------------------------------------------------------------------------

/// One file a [`write_vpk`] fixture puts into the VPK.
#[derive(Debug, Clone)]
pub struct BuildFile {
    path: String,
    data: Vec<u8>,
    preload: Vec<u8>,
    in_dir: bool,
}

impl BuildFile {
    /// A file with archive data: `in_dir` stores it behind the tree in the `_dir.vpk`
    /// (`0x7FFF`), `false` in the sibling `pak01_000.vpk`. Read = preload ++ data.
    pub fn loaded(path: &str, data: &[u8], preload: &[u8], in_dir: bool) -> BuildFile {
        BuildFile {
            path: path.to_string(),
            data: data.to_vec(),
            preload: preload.to_vec(),
            in_dir,
        }
    }

    /// A preload-only entry (the real install's shape: `0x7FFF`, sentinel offset, length 0).
    pub fn preload_only(path: &str, preload: &[u8]) -> BuildFile {
        BuildFile {
            path: path.to_string(),
            data: Vec::new(),
            preload: preload.to_vec(),
            in_dir: false,
        }
    }
}

/// What [`write_vpk`] wrote: the directory file and every sibling archive.
#[derive(Debug)]
pub struct BuiltVpk {
    pub dir: std::path::PathBuf,
    pub archives: Vec<std::path::PathBuf>,
}

/// Write a synthetic `<into>/<base>_dir.vpk` (+ `<base>_000.vpk` when any file keeps its data
/// out of the directory file), parseable by `hammer_mcp::vpk`.
pub fn write_vpk(
    into: &std::path::Path,
    base: &str,
    version: u32,
    files: &[BuildFile],
) -> BuiltVpk {
    let mut tree = Vec::new();
    let mut archives: Vec<std::path::PathBuf> = Vec::new();
    let mut sibling_data: Vec<u8> = Vec::new();
    let mut in_dir_data: Vec<u8> = Vec::new();

    for file in files {
        let last_slash = file.path.rfind('/').map(|i| i + 1).unwrap_or(0);
        let (dir_part, file_name) = file.path.split_at(last_slash);
        let (name, ext) = match file_name.rfind('.') {
            Some(dot) => (&file_name[..dot], &file_name[dot + 1..]),
            None => (file_name, ""),
        };
        let dir_text = if dir_part.is_empty() {
            " "
        } else {
            &dir_part[..dir_part.len() - 1]
        };
        let ext_text = if ext.is_empty() { " " } else { ext };

        let (archive, offset, length) = if file.in_dir {
            (0x7FFFu16, in_dir_data.len() as u32, file.data.len() as u32)
        } else if !file.data.is_empty() {
            (0u16, sibling_data.len() as u32, file.data.len() as u32)
        } else {
            // preload-only: the real install's shape (0x7FFF + the sentinel offset, length 0)
            (0x7FFFu16, u32::MAX, 0)
        };

        tree.extend_from_slice(ext_text.as_bytes());
        tree.push(0);
        tree.extend_from_slice(dir_text.as_bytes());
        tree.push(0);
        tree.extend_from_slice(name.as_bytes());
        tree.push(0);
        tree.extend_from_slice(&0u32.to_le_bytes()); // crc (the reader does not re-check it)
        tree.extend_from_slice(&(file.preload.len() as u16).to_le_bytes());
        tree.extend_from_slice(&archive.to_le_bytes());
        tree.extend_from_slice(&offset.to_le_bytes());
        tree.extend_from_slice(&length.to_le_bytes());
        tree.extend_from_slice(&0xFFFFu16.to_le_bytes());
        tree.extend_from_slice(&file.preload);
        tree.push(0); // the name list's terminator
        tree.push(0); // the dir list's terminator

        if file.in_dir {
            in_dir_data.extend_from_slice(&file.data);
        } else if !file.data.is_empty() {
            sibling_data.extend_from_slice(&file.data);
        }
    }
    tree.push(0); // the ext list's terminator

    let mut out = Vec::new();
    out.extend_from_slice(&0x55AA_1234u32.to_le_bytes());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(tree.len() as u32).to_le_bytes());
    if version == 2 {
        out.extend_from_slice(&(in_dir_data.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // archive md5 section
        out.extend_from_slice(&0u32.to_le_bytes()); // other md5 section
        out.extend_from_slice(&0u32.to_le_bytes()); // signature section
    }
    out.extend_from_slice(&tree);
    out.extend_from_slice(&in_dir_data);

    std::fs::create_dir_all(into).expect("vpk directory");
    let dir = into.join(format!("{base}_dir.vpk"));
    std::fs::write(&dir, &out).expect("dir vpk");
    if !sibling_data.is_empty() {
        let sibling_path = into.join(format!("{base}_000.vpk"));
        std::fs::write(&sibling_path, &sibling_data).expect("sibling vpk");
        archives.push(sibling_path);
    }
    BuiltVpk { dir, archives }
}

// -------------------------------------------------------------------------------------------
// Synthetic compiled model builder (WP-R1: tests/model_info.rs + tests/props_live.rs). A
// `.vmdl_c` is a resource container whose `DATA`/`MDAT`/`ASEQ`/`PHYS` blocks are binary KV3
// (version 4, `Compression::None` here — the round trips in `tests/resource.rs` cover the
// compressed corners); the encoder below is that file's v4-uncompressed path, reduced to the
// value kinds a model fixture uses (no blobs, no version 5, no aux lanes).
// -------------------------------------------------------------------------------------------

/// The "generic" KV3 format GUID as the file stores it (the `tests/resource.rs` constant).
const GENERIC_KV3_FORMAT: [u8; 16] = [
    0x7C, 0x16, 0x12, 0x74, 0xE9, 0x06, 0x98, 0x46, 0xAF, 0xF2, 0xE6, 0x3E, 0xB5, 0x90, 0x37, 0xE7,
];

fn kv3_align(offset: usize, alignment: usize) -> usize {
    (offset + alignment - 1) & !(alignment - 1)
}

/// One binary-KV3 **version 4, uncompressed** block (the `tests/resource.rs` v4 path, minus
/// compression/aux/blobs). Blob and flagged values panic the builder: a model fixture never
/// carries one.
pub fn encode_kv3_v4(root: &Kv3Value) -> Vec<u8> {
    struct Enc {
        b4: Vec<u8>,
        b8: Vec<u8>,
        types: Vec<u8>,
        strings: Vec<String>,
    }
    impl Enc {
        fn string_id(&mut self, text: &str) -> i32 {
            if text.is_empty() {
                return -1; // the writer's id for the empty string
            }
            if let Some(index) = self.strings.iter().position(|existing| existing == text) {
                return index as i32;
            }
            self.strings.push(text.to_owned());
            (self.strings.len() - 1) as i32
        }
        fn lane32(&mut self, value: i32) {
            self.b4.extend_from_slice(&value.to_le_bytes());
        }
        fn lane64(&mut self, bits: u64) {
            self.b8.extend_from_slice(&bits.to_le_bytes());
        }
        /// Pure: the standalone type byte of `value` (the `tests/resource.rs` constants).
        fn type_of(&self, value: &Kv3Value) -> u8 {
            match value {
                Kv3Value::Null => 1,
                Kv3Value::Bool(true) => 13,
                Kv3Value::Bool(false) => 14,
                Kv3Value::Int(0) => 15,
                Kv3Value::Int(1) => 16,
                Kv3Value::Int(number) => {
                    if i32::try_from(*number).is_ok() {
                        11 // T_INT32
                    } else {
                        3 // T_INT64
                    }
                }
                // T_UINT32_ZERO / T_UINT32_ONE: what the current resourcecompiler writes
                Kv3Value::UInt(0) => 26,
                Kv3Value::UInt(1) => 27,
                Kv3Value::UInt(_) => 12,       // T_UINT32 (fixture counters)
                Kv3Value::Double(_) => 5,      // T_DOUBLE
                Kv3Value::String(_) => 6,      // T_STRING
                Kv3Value::Array(_) => 8,       // T_ARRAY
                Kv3Value::TypedArray(_) => 10, // T_ARRAY_TYPED
                Kv3Value::Object(_) => 9,      // T_OBJECT
                Kv3Value::Blob(_) | Kv3Value::Flagged { .. } => {
                    panic!("the model fixture builder does not encode blobs/flagged values")
                }
            }
        }
        /// Everything the value carries besides its leading type byte (which the caller wrote).
        fn body(&mut self, value: &Kv3Value) {
            match value {
                Kv3Value::Null
                | Kv3Value::Bool(_)
                | Kv3Value::Int(0)
                | Kv3Value::Int(1)
                | Kv3Value::UInt(0)
                | Kv3Value::UInt(1) => {}
                Kv3Value::Int(number) => {
                    // 0/1 never get here (their type bytes carry the value); a fixture-length
                    // integer goes into the i32 lane unless it needs i64
                    match i32::try_from(*number) {
                        Ok(number) => self.lane32(number),
                        Err(_) => self.lane64(*number as u64),
                    }
                }
                Kv3Value::UInt(number) => match u32::try_from(*number) {
                    Ok(number) => self.lane32(number as i32),
                    Err(_) => self.lane64(*number),
                },
                Kv3Value::Double(number) => self.lane64(number.to_bits()),
                Kv3Value::String(text) => {
                    let id = self.string_id(text);
                    self.lane32(id);
                }
                Kv3Value::Array(elements) => {
                    self.lane32(elements.len() as i32);
                    for element in elements {
                        self.value(element);
                    }
                }
                Kv3Value::TypedArray(elements) => {
                    self.lane32(elements.len() as i32);
                    // One subtype byte covers every element; the elements carry no type byte.
                    // A mixed uint array cannot use the 0/1 constant subtypes: it goes to the lane.
                    let subtype = self.type_of(&elements[0]);
                    if matches!(subtype, 26 | 27) && elements.iter().any(|e| self.type_of(e) != subtype) {
                        self.types.push(12);
                        for element in elements {
                            let Kv3Value::UInt(number) = element else { panic!("mixed typed array") };
                            self.lane32(*number as i32);
                        }
                    } else {
                        self.types.push(subtype);
                        for element in elements {
                            self.body(element);
                        }
                    }
                }
                Kv3Value::Object(members) => {
                    self.lane32(members.len() as i32);
                    for (name, member) in members {
                        let id = self.string_id(name);
                        self.lane32(id);
                        self.types.push(self.type_of(member));
                        self.body(member);
                    }
                }
                Kv3Value::Blob(_) | Kv3Value::Flagged { .. } => {
                    panic!("the model fixture builder does not encode blobs/flagged values")
                }
            }
        }
        /// A value with its leading type byte.
        fn value(&mut self, value: &Kv3Value) {
            let type_ = self.type_of(value);
            self.types.push(type_);
            self.body(value);
        }
    }
    // Before version 5 the 4-byte lane starts with the string count (patched once the table
    // is known): `tests/resource.rs`'s encoder seeds it as `vec![0, 0, 0, 0]`.
    let mut enc = Enc {
        b4: vec![0, 0, 0, 0],
        b8: Vec::new(),
        types: Vec::new(),
        strings: Vec::new(),
    };
    enc.value(root);

    // The buffer: `[b1(=none)][b4][align 8][b8]` (the v4 writer aligns the 8-byte lane even
    // when it is absent) + the string table + the type stream + the trailer.
    let mut buffer: Vec<u8> = Vec::new();
    let b4_start = kv3_align(0, 4); // b1 is empty, so the b4 lane starts at offset 0
    buffer.extend_from_slice(&enc.b4);
    buffer.resize(kv3_align(buffer.len(), 8), 0);
    buffer.extend_from_slice(&enc.b8);
    buffer[b4_start..b4_start + 4].copy_from_slice(&(enc.strings.len() as i32).to_le_bytes());
    let strings_start = buffer.len();
    for text in &enc.strings {
        buffer.extend_from_slice(text.as_bytes());
        buffer.push(0);
    }
    buffer.extend_from_slice(&enc.types);
    // the type stream's end is this far past the string table's start (strings + types only;
    // the trailer is NOT part of it)
    let count_types = buffer.len() - strings_start;
    buffer.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes()); // the trailer (no blobs)

    let mut out = Vec::new();
    out.extend_from_slice(&0x4B56_3304u32.to_le_bytes()); // magic + version 4
    out.extend_from_slice(&GENERIC_KV3_FORMAT);
    out.extend_from_slice(&0u32.to_le_bytes()); // compression: none
    out.extend_from_slice(&0u16.to_le_bytes()); // dictionary id
    out.extend_from_slice(&0u16.to_le_bytes()); // LZ4 frame size (unused uncompressed)
    out.extend_from_slice(&0i32.to_le_bytes()); // count_bytes1
    out.extend_from_slice(&((enc.b4.len() / 4) as i32).to_le_bytes());
    out.extend_from_slice(&((enc.b8.len() / 8) as i32).to_le_bytes());
    out.extend_from_slice(&(count_types as i32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // object hint
    out.extend_from_slice(&0u16.to_le_bytes()); // array hint
    out.extend_from_slice(&(buffer.len() as i32).to_le_bytes()); // size uncompressed
    out.extend_from_slice(&(buffer.len() as i32).to_le_bytes()); // size "compressed" (none)
    out.extend_from_slice(&0i32.to_le_bytes()); // count_blobs
    out.extend_from_slice(&0i32.to_le_bytes()); // size_blobs
    out.extend_from_slice(&0i32.to_le_bytes()); // count_bytes2 (no test 2-byte lane)
    out.extend_from_slice(&0i32.to_le_bytes()); // size_frame_sizes
    out.extend_from_slice(&buffer);
    out
}

/// One resource container holding `(kind, body)` blocks; bodies are 4-byte aligned in the file
/// (the layout `tests/resource.rs`'s `container` writes, with a string `kind`).
pub fn model_container(blocks: &[(&str, &[u8])]) -> Vec<u8> {
    let table_len = blocks.len() * 12;
    let mut entries = Vec::new();
    let mut bodies: Vec<u8> = Vec::new();
    for (index, (kind, body)) in blocks.iter().enumerate() {
        let field_pos = 20 + index * 12; // the offset field of table entry `index`
        let body_abs = 16 + table_len + bodies.len();
        let mut kind4 = [b' ', 0, 0, 0];
        kind4
            .iter_mut()
            .zip(kind.as_bytes())
            .for_each(|(slot, byte)| *slot = *byte);
        entries.push((kind4, (body_abs - field_pos) as u32, body.len() as u32));
        bodies.extend_from_slice(body);
        bodies.resize(bodies.len().next_multiple_of(4), 0);
    }
    let size = 16 + table_len + bodies.len();
    let mut out = Vec::new();
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&12u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    for (kind, rel, len) in entries {
        out.extend_from_slice(&kind);
        out.extend_from_slice(&rel.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(&bodies);
    out
}

/// A synthetic `worldmodel.vnmskel_c` with the real file's 75 bones and both reference poses
/// ([`vnmskel_data`], taken from the SPIKE-I0 dump): a resource container with one `DATA` block.
pub fn vnmskel_c() -> Vec<u8> {
    use vnmskel_data::{LOCAL, MODEL, NAMES, PARENTS};
    let pose_list = |poses: &[[f64; 8]]| {
        Kv3Value::Array(poses.iter().map(|p| Kv3Value::TypedArray(p.iter().map(|v| Kv3Value::Double(*v)).collect())).collect())
    };
    let root = kv_object(vec![
        ("m_ID", kv_text("animation/skeletons/characters/worldmodel.vnmskel")),
        ("m_boneIDs", kv_strings(&NAMES)),
        ("m_parentIndices", Kv3Value::TypedArray(PARENTS.iter().map(|p| Kv3Value::Double(*p as f64)).collect())),
        ("m_parentSpaceReferencePose", pose_list(&LOCAL)),
        ("m_modelSpaceReferencePose", pose_list(&MODEL)),
    ]);
    model_container(&[("DATA", &encode_kv3_v4(&root))])
}

/// A synthetic agent `.vmdl_c` ([`agent_data`], numbers from the real `ctm_sas.vmdl_c`): one `MDAT`
/// block with `m_attachments` + `m_hitboxsets` in the compiled layout and a `DATA` block whose
/// `m_modelSkeleton` holds the bones down to `jiggle_primary`.
pub fn agent_vmdl_c() -> Vec<u8> {
    use agent_data::{ATTACHMENTS, BONES, HITBOXES, HITBOX_SET};
    let doubles = |v: &[f64]| Kv3Value::TypedArray(v.iter().map(|x| Kv3Value::Double(*x)).collect());
    let attachments = ATTACHMENTS
        .iter()
        .map(|(name, ignore, bone, offset, rot, weight, root)| {
            kv_object(vec![
                ("key", kv_text(name)),
                (
                    "value",
                    kv_object(vec![
                        ("m_bIgnoreRotation", Kv3Value::Bool(*ignore)),
                        ("m_bInfluenceRootTransform", Kv3Value::TypedArray(vec![Kv3Value::Bool(*root), Kv3Value::Bool(false), Kv3Value::Bool(false)])),
                        ("m_influenceNames", kv_strings(&[bone, "", ""])),
                        ("m_influenceWeights", doubles(&[*weight, 0.0, 0.0])),
                        ("m_nInfluences", Kv3Value::Int(1)),
                        ("m_name", kv_text(name)),
                        ("m_vInfluenceOffsets", Kv3Value::Array(vec![doubles(offset), doubles(&[0.0; 3]), doubles(&[0.0; 3])])),
                        ("m_vInfluenceRotations", Kv3Value::Array(vec![doubles(rot), doubles(&[0.0, 0.0, 0.0, 1.0]), doubles(&[0.0, 0.0, 0.0, 1.0])])),
                    ]),
                ),
            ])
        })
        .collect();
    let hitboxes = HITBOXES
        .iter()
        .enumerate()
        .map(|(i, (name, bone, shape, radius, min, max, surface, group, translation))| {
            kv_object(vec![
                ("m_bTranslationOnly", Kv3Value::Bool(*translation)),
                ("m_flShapeRadius", Kv3Value::Double(*radius)),
                ("m_nGroupId", Kv3Value::Int(*group)),
                ("m_nHitBoxIndex", Kv3Value::Int(i as i64)),
                ("m_nShapeType", Kv3Value::Int(*shape)),
                ("m_name", kv_text(name)),
                ("m_sBoneName", kv_text(bone)),
                ("m_sSurfaceProperty", kv_text(surface)),
                ("m_vMaxBounds", doubles(max)),
                ("m_vMinBounds", doubles(min)),
            ])
        })
        .collect();
    let mdat = kv_object(vec![
        ("_class", kv_text("PermModelData_t")),
        ("m_attachments", Kv3Value::Array(attachments)),
        (
            "m_hitboxsets",
            Kv3Value::Array(vec![kv_object(vec![
                ("key", kv_text(HITBOX_SET)),
                ("value", kv_object(vec![("m_HitBoxes", Kv3Value::Array(hitboxes)), ("m_name", kv_text(HITBOX_SET))])),
            ])]),
        ),
    ]);
    let names: Vec<&str> = BONES.iter().map(|b| b.0).collect();
    let data = kv_object(vec![
        ("m_name", kv_text("agents/models/ctm_sas/ctm_sas.vmdl")),
        (
            "m_modelSkeleton",
            kv_object(vec![
                ("m_boneName", kv_strings(&names)),
                ("m_nParent", Kv3Value::TypedArray(BONES.iter().map(|b| Kv3Value::Double(b.1 as f64)).collect())),
                ("m_bonePosParent", Kv3Value::Array(BONES.iter().map(|b| doubles(&b.2)).collect())),
                ("m_boneRotParent", Kv3Value::Array(BONES.iter().map(|b| doubles(&b.3)).collect())),
            ]),
        ),
    ]);
    model_vmdl_c(&[("DATA", data), ("MDAT", mdat)])
}

/// A minimal compiled material: a resource container with only an `RERL` (external-reference list)
/// block naming `textures` (uncompiled `.vtex` spellings), in ValveResourceFormat's layout:
/// `u32 offset` (from the block start), `u32 count`, then `count` x (`u64 id`, `i64` offset to the
/// NUL-terminated name, relative to that offset field), the names after the entries.
pub fn vmat_c(textures: &[&str]) -> Vec<u8> {
    vmat_c_claiming(textures, textures.len() as u32)
}

/// [`vmat_c`] whose list header claims `claimed` entries while only `textures` are present: a
/// truncated reference list (`claimed > textures.len()`).
pub fn vmat_c_claiming(textures: &[&str], claimed: u32) -> Vec<u8> {
    let entries_at = 8usize;
    let mut names_at = entries_at + textures.len() * 16;
    let mut block = Vec::new();
    block.extend_from_slice(&(entries_at as u32).to_le_bytes());
    block.extend_from_slice(&claimed.to_le_bytes());
    let mut names = Vec::new();
    for (i, tex) in textures.iter().enumerate() {
        let field = entries_at + i * 16 + 8;
        block.extend_from_slice(&(i as u64 + 1).to_le_bytes());
        block.extend_from_slice(&((names_at - field) as i64).to_le_bytes());
        names.extend_from_slice(tex.as_bytes());
        names.push(0);
        names_at += tex.len() + 1;
    }
    block.extend_from_slice(&names);
    model_container(&[("RERL", &block)])
}

// The fixture value-tree shorthands (same shapes as `tests/resource.rs`'s).
pub fn kv_object(members: Vec<(&str, Kv3Value)>) -> Kv3Value {
    Kv3Value::Object(
        members
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}

pub fn kv_text(value: &str) -> Kv3Value {
    Kv3Value::String(value.to_owned())
}

/// A `[x, y, z]` typed array of doubles (what compiled `DATA`/`MDAT` bounds use).
pub fn kv_vec3(x: f64, y: f64, z: f64) -> Kv3Value {
    Kv3Value::TypedArray(vec![
        Kv3Value::Double(x),
        Kv3Value::Double(y),
        Kv3Value::Double(z),
    ])
}

pub fn kv_strings(values: &[&str]) -> Kv3Value {
    Kv3Value::TypedArray(values.iter().map(|v| kv_text(v)).collect())
}

/// A whole synthetic `*.vmdl_c`: `(kind, KV3 root)` blocks in one container.
pub fn model_vmdl_c(blocks: &[(&str, Kv3Value)]) -> Vec<u8> {
    let encoded: Vec<(String, Vec<u8>)> = blocks
        .iter()
        .map(|(kind, body)| ((*kind).to_string(), encode_kv3_v4(body)))
        .collect();
    let refs: Vec<(&str, &[u8])> = encoded
        .iter()
        .map(|(kind, bytes)| (kind.as_str(), bytes.as_slice()))
        .collect();
    model_container(&refs)
}

// -------------------------------------------------------------------------------------------
// Soundevent fixtures (WP-S1: tests/soundevents_index.rs + tests/soundevents_query.rs). A
// `.vsndevts_c` is a resource container with one binary-KV3 `DATA` block whose root is
// `eventName -> object`, so the compiled fixture is [`write_vsndevts_c`] of a KV3 tree; the
// addon's `.vsndevts` source is KV3 text ([`SoundTree`] writes it).
// -------------------------------------------------------------------------------------------

/// A compiled `*.vsndevts_c`: one `DATA` block holding the KV3 event tree.
pub fn write_vsndevts_c(root: &Kv3Value) -> Vec<u8> {
    model_container(&[("DATA", &encode_kv3_v4(root))])
}

/// Set `path`'s mtime `seconds` into the future: a *deterministic* "this file changed" signal
/// for the freshness rules, independent of the filesystem's timestamp resolution (a rewrite in
/// the same nanosecond would otherwise leave `fresh()` true).
pub fn bump_mtime(path: &std::path::Path, seconds: u64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("{} for mtime: {e}", path.display()));
    let when = std::time::SystemTime::now() + std::time::Duration::from_secs(seconds);
    file.set_modified(when)
        .unwrap_or_else(|e| panic!("set mtime of {}: {e}", path.display()));
}

/// The addon name every sound fixture uses (the `--addon` default).
pub const SOUND_ADDON: &str = "mcp_test";
/// The fixture's VPK blocks (the three `soundevents/*.vsndevts_c` the WP names).
pub const SND_CSGO_BLOCK: &str = "soundevents/soundevents_weapons.vsndevts_c";
pub const SND_CSGO_AMBIENCE_BLOCK: &str =
    "soundevents/ambience/game_sounds_amb_common.vsndevts_c";
pub const SND_CORE_BLOCK: &str = "soundevents/soundevents_soundscapes_core.vsndevts_c";

/// The addon's `.vsndevts` text: `mcp.src.only` exists only here (so it is `compiled:false`) and
/// `mcp.both` is also in the compiled file (so it is `compiled:true` with two sources).
pub const SND_ADDON_SOURCE_TEXT: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\t\"mcp.src.only\" =\n\t{\n\t\ttype = \"csgo_mega\"\n\t\tvsnd_files_track_01 = \"sounds/only.vsnd\"\n\t}\n\t\"mcp.both\" =\n\t{\n\t\ttype = \"csgo_mega\"\n\t\tvolume = 0.9\n\t}\n}\n";

/// The compiled addon `soundevents_addon.vsndevts_c` tree: `mcp.both` and a copy of
/// `Shared.Precedence` (which the csgo pak also carries — the precedence pin: the addon's
/// `volume` 0.25 wins over the pak's 0.5).
fn compiled_addon_root() -> Kv3Value {
    kv_object(vec![
        (
            "mcp.both",
            kv_object(vec![
                ("type", kv_text("csgo_mega")),
                ("volume", Kv3Value::Double(0.9)),
            ]),
        ),
        (
            "Shared.Precedence",
            kv_object(vec![
                ("type", kv_text("csgo_mega")),
                ("volume", Kv3Value::Double(0.25)),
            ]),
        ),
    ])
}

/// A synthetic app tree (`workshop/run/app`) with the soundevent fixture in it: a csgo and a
/// core `pak01_dir.vpk` carrying three `soundevents/*.vsndevts_c` blocks between them, the
/// addon's compiled `_c` and its `.vsndevts` text, and a `sounds/` asset on each side. Removed
/// on drop; never under `/tmp` (`CARGO_TARGET_TMPDIR`).
pub struct SoundTree {
    pub root: std::path::PathBuf,
    pub app: std::path::PathBuf,
    pub addon: String,
}

impl SoundTree {
    /// The whole default fixture (see the module notes on this section).
    pub fn new(tag: &str) -> SoundTree {
        let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "sound-tree-{tag}-{}-{}",
            std::process::id(),
            TRAIL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let app = root.join("workshop/run/app");
        std::fs::create_dir_all(&app).expect("temp tree");
        let tree = SoundTree {
            root,
            app,
            addon: SOUND_ADDON.to_string(),
        };

        // csgo pak: the weapons block (a string vsnd, the cross-source name, the base cycle, the
        // suggestion pair) + the ambience block (an array child list and array vsnds)
        write_vpk(
            &tree.app.join("game/csgo"),
            "pak01",
            2,
            &[
                BuildFile::loaded(
                    SND_CSGO_BLOCK,
                    &write_vsndevts_c(&csgo_weapons_root()),
                    b"",
                    true,
                ),
                BuildFile::loaded(
                    SND_CSGO_AMBIENCE_BLOCK,
                    &write_vsndevts_c(&csgo_ambience_root()),
                    b"",
                    false,
                ),
                // the asset Weapon_AK47.Single references (the compiled spelling: the check
                // looks for `<path>_c` too)
                BuildFile::preload_only("sounds/weapons/ak47/ak47-1.vsnd_c", b""),
            ],
        );
        // core pak: the `amb.*.base` templates (the plan's core-pak templates)
        write_vpk(
            &tree.app.join("game/core"),
            "pak01",
            1,
            &[BuildFile::loaded(
                SND_CORE_BLOCK,
                &write_vsndevts_c(&core_templates_root()),
                b"",
                false,
            )],
        );

        // the two addon sides
        tree.write_compiled(&write_vsndevts_c(&compiled_addon_root()));
        tree.write_source(SND_ADDON_SOURCE_TEXT);
        // the asset mcp.src.only references (the addon's own sounds tree)
        let content_sounds = tree
            .app
            .join("content/csgo_addons")
            .join(&tree.addon)
            .join("sounds");
        std::fs::create_dir_all(&content_sounds).expect("addon sounds dir");
        std::fs::write(content_sounds.join("only.vsnd"), b"fake vsnd").expect("addon vsnd");
        tree
    }

    pub fn app(&self) -> std::path::PathBuf {
        self.app.clone()
    }

    /// `<app>/game/csgo/pak01_dir.vpk`.
    pub fn csgo_vpk(&self) -> std::path::PathBuf {
        self.app.join("game/csgo/pak01_dir.vpk")
    }

    /// `<app>/game/core/pak01_dir.vpk`.
    pub fn core_vpk(&self) -> std::path::PathBuf {
        self.app.join("game/core/pak01_dir.vpk")
    }

    /// `<app>/game/csgo_addons/<addon>/soundevents/soundevents_addon.vsndevts_c`.
    pub fn addon_compiled(&self) -> std::path::PathBuf {
        self.app
            .join("game/csgo_addons")
            .join(&self.addon)
            .join("soundevents/soundevents_addon.vsndevts_c")
    }

    /// `<app>/content/csgo_addons/<addon>/soundevents/soundevents_addon.vsndevts`.
    pub fn addon_source(&self) -> std::path::PathBuf {
        self.app
            .join("content/csgo_addons")
            .join(&self.addon)
            .join("soundevents/soundevents_addon.vsndevts")
    }

    /// The addon's compiled soundevents directory.
    pub fn addon_compiled_dir(&self) -> std::path::PathBuf {
        self.app
            .join("game/csgo_addons")
            .join(&self.addon)
            .join("soundevents")
    }

    /// The addon's source soundevents directory.
    pub fn addon_source_dir(&self) -> std::path::PathBuf {
        self.app
            .join("content/csgo_addons")
            .join(&self.addon)
            .join("soundevents")
    }

    /// Overwrite the addon's compiled `_c` (any bytes: the corrupt-file case uses garbage).
    pub fn write_compiled(&self, bytes: &[u8]) {
        let path = self.addon_compiled();
        std::fs::create_dir_all(path.parent().expect("addon dir")).expect("addon dir");
        std::fs::write(&path, bytes).expect("addon _c");
    }

    /// Overwrite the addon's `.vsndevts` text (the unparseable case uses a broken text).
    pub fn write_source(&self, text: &str) {
        let path = self.addon_source();
        std::fs::create_dir_all(path.parent().expect("addon dir")).expect("addon dir");
        std::fs::write(&path, text).expect("addon source");
    }

    /// Write one more `.vsndevts` into the addon's source directory (a file appearing
    /// mid-session; `rescan_addon` must see it).
    pub fn add_source_file(&self, name: &str, text: &str) -> std::path::PathBuf {
        let path = self.addon_source_dir().join(name);
        std::fs::write(&path, text).expect("extra addon source");
        path
    }
}

impl Drop for SoundTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The csgo pak's weapons block: a string vsnd, the name the addon also carries, the base cycle
/// and the two names the suggestion ranking test needs.
fn csgo_weapons_root() -> Kv3Value {
    let mega = |members: Vec<(&str, Kv3Value)>| {
        let mut all = vec![("type", kv_text("csgo_mega"))];
        all.extend(members);
        kv_object(all)
    };
    kv_object(vec![
        (
            "Weapon_AK47.Single",
            mega(vec![
                ("volume", Kv3Value::Double(1.0)),
                (
                    "vsnd_files_track_01",
                    kv_text("sounds/weapons/ak47/ak47-1.vsnd"),
                ),
            ]),
        ),
        (
            "Shared.Precedence",
            mega(vec![
                ("volume", Kv3Value::Double(0.5)),
                ("vsnd_files_track_01", kv_text("sounds/game.vsnd")),
            ]),
        ),
        ("cycle.a", mega(vec![("base", kv_text("cycle.b"))])),
        ("cycle.b", mega(vec![("base", kv_text("cycle.a"))])),
        ("snd.near", mega(vec![])),
        ("snd.near.long", mega(vec![])),
    ])
}

/// The csgo pak's ambience block: the array child list and the string-or-array vsnds.
fn csgo_ambience_root() -> Kv3Value {
    kv_object(vec![
        (
            "ambient_fx.child",
            kv_object(vec![
                ("type", kv_text("csgo_mega")),
                (
                    "soundevent_01",
                    kv_strings(&["ambient_fx.child.a", "ambient_fx.child.b"]),
                ),
                (
                    "vsnd_files_track_01",
                    kv_text("sounds/fx/fx_01.vsnd"),
                ),
                ("vsnd_files", kv_strings(&["sounds/fx/a.vsnd", "sounds/fx/b.vsnd"])),
            ]),
        ),
        (
            "ambient_fx.child.a",
            kv_object(vec![("type", kv_text("csgo_mega"))]),
        ),
        (
            "ambient_fx.child.b",
            kv_object(vec![("type", kv_text("csgo_mega"))]),
        ),
        // a dangling child in the STRING form (the `known:false` case for the detail route)
        (
            "ambient_fx.orphan",
            kv_object(vec![
                ("type", kv_text("csgo_mega")),
                ("soundevent_01", kv_text("ambient_fx.missing")),
            ]),
        ),
    ])
}

/// The core pak's block: the `amb.*.base` templates (one with a `position`).
fn core_templates_root() -> Kv3Value {
    kv_object(vec![
        (
            "amb.base",
            kv_object(vec![
                ("type", kv_text("csgo_ambient")),
                ("volume", Kv3Value::Double(1.0)),
            ]),
        ),
        (
            "amb.looping.atXYZ.base",
            kv_object(vec![
                ("base", kv_text("amb.base")),
                ("volume", Kv3Value::Double(0.8)),
                ("position", kv_vec3(1.0, 2.0, 3.0)),
            ]),
        ),
    ])
}
