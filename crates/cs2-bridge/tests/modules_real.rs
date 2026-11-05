//! The module identity gate over the **real** game binaries: `verify_one` for every
//! `PIN_MODULES` row, reading the DLLs from the staged bins tree, plus the negative (a copied
//! DLL with one byte flipped must fail closed).
//!
//! Like `tests/kv3_corpus.rs` this needs files that are not in git (Valve's DLLs under
//! `target/game-bins`, gitignored), so it is its own test target with `test = false` in
//! `Cargo.toml`:
//!
//! ```text
//! cargo test -p cs2-bridge --test modules_real -- --nocapture
//! ```
//!
//! A missing tree or a missing module fails loudly, naming what is missing - this test never
//! skips itself.
//!
//! # File offsets versus the loader's layout
//!
//! `verify_one` reads a **mapped image** (index = RVA), which is what `verify_one` sees on
//! Windows. Every check it runs is RVA-based, and none of them is equivalent at a file offset,
//! because `FileAlignment` puts each section's raw data at a different offset than its
//! `VirtualAddress`:
//!
//! * `pe_info` reads the DOS/PE/optional headers (RVA 0, which *is* file offset 0) and then the
//!   debug directory at `DataDirectory[IMAGE_DIRECTORY_ENTRY_DEBUG].VirtualAddress` and the RSDS
//!   record at `IMAGE_DEBUG_DIRECTORY::AddressOfRawData` - both RVAs, both usually in `.rdata`.
//! * the byte pins read `base + pin.rva`, i.e. RVAs inside `.text`.
//! * the slot pins read vtable words at `base + vt_rva`, RVAs inside `.rdata`; their expected
//!   value is `base + target_rva`, which the *file* holds as `ImageBase + target_rva` (the
//!   loader adds `actual_base - ImageBase` to it, so the comparison holds either way). The test
//!   therefore uses the file's own `ImageBase` as the base, read from its optional header.
//!
//! [`cs2_bridge::pe::map_image`] builds exactly that layout: headers at RVA 0, each section at
//! its `VirtualAddress`, gaps zero. It is not a substitute for the loader in general (it does
//! not apply base relocations), but with `base = ImageBase` the file's pointers are already the
//! values the loader would write for a preferred-base load, and these modules are linked for
//! `0x180000000`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cs2_bridge::generated::pins::{PIN_BYTES, PIN_MODULES};
use cs2_bridge::modules::{FileHash, Image, Modules, Pins, Platform, State, verify_one};
use cs2_bridge::pe;

/// The pinned modules' file names, found anywhere under the bins tree (the install layout puts
/// public modules under `game/bin/win64`, the client/server ones under `csgo/bin/win64` and the
/// tools' ones under `game/bin/win64/tools`).
struct Bins {
    paths: HashMap<&'static str, PathBuf>,
}

impl Bins {
    /// Walk `root` and collect every pinned module name. A module that is not there is a hard
    /// failure naming it.
    fn load(root: &Path) -> Self {
        assert!(
            root.is_dir(),
            "the real-binary gate needs the staged bins tree at {} (sigs/README.md \"Binaries\": \
             python3 sigs/scripts/stagebins.py --game workshop/run/app --out target/game-bins); \
             set CS2_BINS to point elsewhere",
            root.display()
        );
        let mut found: HashMap<String, PathBuf> = HashMap::new();
        walk(root, &mut found);
        let mut paths = HashMap::new();
        let mut missing = Vec::new();
        for module in PIN_MODULES {
            match found.get(&module.name.to_ascii_lowercase()) {
                Some(path) => {
                    paths.insert(module.name, path.clone());
                }
                None => missing.push(module.name),
            }
        }
        assert!(
            missing.is_empty(),
            "the bins tree {} is missing: {}",
            root.display(),
            missing.join(", ")
        );
        Self { paths }
    }

    fn bytes(&self, name: &str) -> Vec<u8> {
        let path = self
            .paths
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not a pinned module"));
        std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    /// The base the file's absolute pointers are written for: `OptionalHeader.ImageBase`.
    fn image_base(&self, name: &str) -> u64 {
        let bytes = self.bytes(name);
        let pe_offset =
            usize::try_from(u32::from_le_bytes(bytes[0x3C..0x40].try_into().expect("4 bytes")))
                .expect("a u32");
        let base = pe_offset + 24 + pe::PE32PLUS_IMAGEBASE;
        u64::from_le_bytes(bytes[base..base + 8].try_into().expect("8 bytes"))
    }
}

/// Every file under `dir` that is named after a pinned module.
fn walk(dir: &Path, found: &mut HashMap<String, PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("a readable directory entry");
        let path = entry.path();
        let metadata = entry
            .metadata()
            .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
        if metadata.is_dir() {
            walk(&path, found);
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        if PIN_MODULES.iter().any(|module| module.name == name) {
            found.insert(name, path);
        }
    }
}

/// The C's loader calls, over the file-backed image: `GetModuleHandleA` is "the file exists and
/// its `ImageBase` is the base", reading the mapped image is [`pe::map_image`],
/// `GetModuleFileNameA` + `sha256_file` is [`pe::sha256`] of the file, and
/// `GetModuleHandleExW(PIN)` has no host equivalent - there is nothing to pin in this process,
/// so [`pin`](Platform::pin) asserts the argument the C would pass (the module's first byte-pin
/// RVA, `bridge/mcp_bridge.c:1352-1356`) and reports success, as it does on Windows.
struct FilePlatform {
    bins: Bins,
    /// The modules the fake loader says are loaded (all of them for the full run).
    loaded: Vec<&'static str>,
}

impl Platform for FilePlatform {
    fn module_base(&self, name: &str) -> Option<u64> {
        self.loaded
            .contains(&name)
            .then(|| self.bins.image_base(name))
    }

    fn image(&self, name: &str, base: u64) -> Option<Vec<u8>> {
        let bytes = self.bins.bytes(name);
        let mapped = pe::map_image(&bytes)
            .unwrap_or_else(|error| panic!("{name} must map as PE32+: {error:?}"));
        assert_eq!(mapped.image_base, base, "{name}: the base is the file's ImageBase");
        Some(mapped.bytes)
    }

    fn file_hash(&self, name: &str, _base: u64) -> FileHash {
        FileHash::Digest(pe::sha256(&self.bins.bytes(name)))
    }

    fn pin(&self, name: &str, _base: u64, rva: u32) -> bool {
        let index = PIN_MODULES
            .iter()
            .position(|module| module.name == name)
            .unwrap_or_else(|| panic!("{name} is not a pinned module"));
        let first = PIN_BYTES
            .iter()
            .find(|pin| usize::from(pin.module) == index)
            .unwrap_or_else(|| panic!("{name} has no byte pin"));
        assert_eq!(rva, first.rva, "{name}: the C pins by the first byte pin's RVA");
        true
    }
}

fn bins_root() -> PathBuf {
    match std::env::var_os("CS2_BINS") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/game-bins"),
    }
}

/// Every `PIN_MODULES` row: PE facts, RSDS, file SHA-256, byte pins and slot pins, against the
/// real DLLs. Prints each module's state and the mask.
#[test]
fn every_real_module_verifies() {
    let bins = Bins::load(&bins_root());
    let names: Vec<&'static str> = PIN_MODULES.iter().map(|module| module.name).collect();
    let modules = Modules::new(FilePlatform {
        bins,
        loaded: names.clone(),
    });

    modules.verify_all();

    for (index, module) in PIN_MODULES.iter().enumerate() {
        let state = modules.state(index as u8).expect("a pin index");
        println!(
            "{:>2} {:24} {:>9} base {:#x}",
            index,
            module.name,
            state.name(),
            modules.base(index as u8)
        );
        assert_eq!(state, State::Verified, "{} must verify", module.name);
    }
    println!("verified_mask = {:#x}", modules.verified_mask());
    assert_eq!(modules.verified_mask(), 0xfff, "every module contributes its bit");
    assert_eq!(modules.caps_revision(), 13, "1 + one bump per module");
    assert!(!modules.verifier_running(), "the verifier finished");

    // And the pins_for rows really are the rows verify_one consumed (same module, table order).
    for (index, module) in PIN_MODULES.iter().enumerate() {
        let (bytes, slots) = cs2_bridge::modules::pins_for(index as u8);
        assert!(
            bytes.iter().all(|pin| usize::from(pin.module) == index),
            "{} byte pins",
            module.name
        );
        assert!(
            slots.iter().all(|pin| usize::from(pin.module) == index),
            "{} slot pins",
            module.name
        );
        assert!(!bytes.is_empty(), "{} has byte pins", module.name);
    }
}

/// The negative: a **copy** of a real DLL under `target/scratch/` with one byte flipped must
/// fail closed with the file-hash mismatch (`verify: file SHA-256 mismatch`), and the unmodified
/// copy must pass - which shows the copy itself is the pinned file.
#[test]
fn a_flipped_byte_in_a_copied_dll_fails_closed() {
    let bins = Bins::load(&bins_root());
    // schemasystem.dll is the smallest pinned module (445 KiB), so the copy is cheap; the flip
    // is in the middle of the file, in no pinned prologue, so the reason under test is the file
    // hash rather than a byte pin.
    let name = "schemasystem.dll";
    let index = PIN_MODULES
        .iter()
        .position(|module| module.name == name)
        .expect("schemasystem.dll is pinned");
    let original = bins.bytes(name);

    let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch/modules_negative");
    std::fs::create_dir_all(&scratch).expect("the scratch directory is writable");
    let copy_path = scratch.join(name);
    std::fs::write(&copy_path, &original).expect("the copy is written");

    let copied = std::fs::read(&copy_path).expect("the copy is readable");
    assert_eq!(
        pe::sha256(&copied),
        PIN_MODULES[index].sha256,
        "the unmodified copy is the pinned file"
    );

    let mut flipped = copied.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 0xff;
    std::fs::write(&copy_path, &flipped).expect("the flipped copy is written");
    assert_ne!(pe::sha256(&flipped), PIN_MODULES[index].sha256);

    // The unmodified copy verifies ...
    let mapped = pe::map_image(&copied).expect("the copy maps");
    let (byte_pins, slot_pins) = cs2_bridge::modules::pins_for(index as u8);
    assert_eq!(
        verify_one(
            Pins {
                identity: &PIN_MODULES[index],
                bytes: &byte_pins,
                slots: &slot_pins,
            },
            Some(&Image::new(&mapped.bytes, mapped.image_base)),
            FileHash::Digest(pe::sha256(&copied)),
            &mut Vec::new(),
            &std::sync::atomic::AtomicBool::new(false),
            |_rva| true,
        ),
        Ok(())
    );

    // ... and the copy with the flipped byte fails with the C's file-hash line.
    let mapped = pe::map_image(&flipped).expect("the flipped copy still maps");
    let failure = verify_one(
        Pins {
            identity: &PIN_MODULES[index],
            bytes: &byte_pins,
            slots: &slot_pins,
        },
        Some(&Image::new(&mapped.bytes, mapped.image_base)),
        FileHash::Digest(pe::sha256(&flipped)),
        &mut Vec::new(),
        &std::sync::atomic::AtomicBool::new(false),
        |_rva| true,
    )
    .expect_err("a flipped byte must not verify");
    println!(
        "{name}: flipped byte at file offset {middle} -> {:?} \"{}\"",
        failure,
        failure.line()
    );
    assert_eq!(failure, cs2_bridge::modules::Fail::FileMismatch);
    assert_eq!(failure.line(), "verify: file SHA-256 mismatch");

    // The whole state machine agrees: this module, loaded from the flipped copy, is `mismatch`
    // and contributes no bit.
    let flipped_copy = scratch.join(name);
    let mut paths = HashMap::new();
    paths.insert(name, flipped_copy);
    let modules = Modules::new(
        SingleFilePlatform { bins: paths, base: mapped.image_base },
    );
    modules.verify_all();
    assert_eq!(modules.state(index as u8), Some(State::Mismatch));
    assert_eq!(modules.verified_mask(), 0);
    println!("{name}: state {} mask {:#x}", modules.state_name(index as u8), modules.verified_mask());
}

/// A one-file loader for the negative: same contract as [`FilePlatform`], a single module.
struct SingleFilePlatform {
    bins: HashMap<&'static str, PathBuf>,
    base: u64,
}

impl Platform for SingleFilePlatform {
    fn module_base(&self, name: &str) -> Option<u64> {
        self.bins.get(name).map(|_path| self.base)
    }

    fn image(&self, name: &str, base: u64) -> Option<Vec<u8>> {
        let path = self.bins.get(name)?;
        let bytes = std::fs::read(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert_eq!(base, self.base);
        Some(pe::map_image(&bytes).expect("the copy maps").bytes)
    }

    fn file_hash(&self, name: &str, _base: u64) -> FileHash {
        let path = self.bins.get(name).expect("a loaded module");
        let bytes = std::fs::read(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        FileHash::Digest(pe::sha256(&bytes))
    }

    fn pin(&self, _name: &str, _base: u64, _rva: u32) -> bool {
        // Unreachable for a mismatching module: the pin is only attempted after every check
        // passed. A `true` here would be a lie, so the negative path asserts it is never called.
        panic!("verify_one must fail before it pins a mismatching module")
    }
}
