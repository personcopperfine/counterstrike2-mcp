//! The module identity gate (`cs2_bridge::modules`) over synthetic images and a fake loader.
//!
//! Everything here is the C block at `mcp_bridge.c:1149-1405`: `verify_one`'s checks and
//! their log lines, the state machine, `verified_mask`, the poll throttle and the
//! single-verifier rule. The synthetic image is a real PE32+ file built in memory and mapped
//! with [`cs2_bridge::pe::map_image`] before it is handed to `verify_one`.
//!
//! The fake loader ([`FakePlatform`]) stands in for the C's four loader calls
//! (`GetModuleHandleA` `:1310`, the mapped image, `GetModuleFileNameA` + `sha256_file`
//! `:1330-1340`, `GetModuleHandleExW(MODULE_PIN_FLAGS, ...)` `:1358`). It mirrors exactly what
//! those return for a build that *is* the pinned one, and asserts the arguments the gate passes
//! (`image` only ever reads inside the image it returned; `pin` only ever gets the base
//! `module_base` returned and the module's first byte-pin RVA, which is what `:1352-1356`
//! computes).

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cs2_bridge::generated::pins::{
    PIN_BYTES, PIN_MODULES, PIN_SLOTS, PIN_TF2, PIN_TIER0, PinBytes, PinModule, PinSlot,
};
use cs2_bridge::modules::{
    Fail, FileHash, Image, LoadedModule, Modules, Pins, Platform, State, need_bit, pins_for,
    verify_one, wait_idle,
};
use cs2_bridge::pe;

// ---------------------------------------------------------------------------------------------
// The synthetic PE32+ image: file -> loader mapping -> the gate.
// ---------------------------------------------------------------------------------------------

/// `IMAGE_DOS_HEADER::e_lfanew` and the PE header (`pe.rs` names the winnt.h facts; the test
/// writes the same layout).
const E_LFANEW: usize = 0x40;
const PE_HDR: usize = E_LFANEW;
const OPT_HDR: usize = PE_HDR + 24;
/// `IMAGE_OPTIONAL_HEADER64::SizeOfOptionalHeader`, the value this image declares.
const OPT_SIZE: u16 = 0xF0;
/// The header block; deliberately smaller than `POINTER_TO_RAW_DATA` so the file has a gap that
/// no RVA maps (which is how the file-hash check is exercised on its own).
const SIZE_OF_HEADERS: u32 = 0x200;
/// The single section.
const SECTION_RVA: u32 = 0x1000;
const SECTION_VIRTUAL_SIZE: u32 = 0x1000;
const SECTION_RAW_SIZE: u32 = 0x1000;
const POINTER_TO_RAW_DATA: u32 = 0x400;
const SIZE_OF_IMAGE: u32 = 0x2000;
/// The pinned function prologue, inside the section.
const PROLOGUE_RVA: u32 = 0x1100;
const PROLOGUE: [u8; 6] = [0x48, 0x89, 0x5c, 0x24, 0x08, 0x57];
/// The pinned vtable and its pinned slot.
const VT_RVA: u32 = 0x1200;
const VT_SLOT: u16 = 3;
const TARGET_RVA: u32 = 0x1300;
/// The CodeView debug directory and its RSDS record, inside the single section but below the
/// smallest pinned RVA in the whole table (0x10398), so no pin row overlaps them.
const DEBUG_RVA: u32 = 0x1800;
const RSDS_RVA: u32 = 0x1900;
/// `IMAGE_OPTIONAL_HEADER64::ImageBase`: the base the file's vtable word is written for.
const BASE: u64 = 0x1_8000_0000;
const TIME_STAMP: u32 = 0x6ac8_001f;
const CHECK_SUM: u32 = 0x025d_45b7;
const GUID: [u8; 16] = [
    0xd4, 0x85, 0xa5, 0xa2, 0xc8, 0x06, 0xa5, 0x47, 0x82, 0x8a, 0xdb, 0xef, 0x47, 0x76, 0xae,
    0xdd,
];
const AGE: u32 = 7;
/// A file byte in the gap between `SIZE_OF_HEADERS` and `POINTER_TO_RAW_DATA`: it is in the file
/// but in no mapped section, so changing it changes only the file SHA-256.
const GAP_BYTE: usize = 0x300;

/// The synthetic pins, in the shape `PIN_BYTES`/`PIN_SLOTS` have (one row each). They are
/// `static` so `Pins` can borrow them for as long as the fixture lives.
static SYNTH_PROLOGUE_PIN: PinBytes = PinBytes {
    module: 0,
    rva: PROLOGUE_RVA,
    len: PROLOGUE.len() as u8,
    bytes: {
        let mut bytes = [0u8; 24];
        bytes[0] = PROLOGUE[0];
        bytes[1] = PROLOGUE[1];
        bytes[2] = PROLOGUE[2];
        bytes[3] = PROLOGUE[3];
        bytes[4] = PROLOGUE[4];
        bytes[5] = PROLOGUE[5];
        bytes
    },
    what: "SYNTH_PROLOGUE",
};
static SYNTH_SLOT_PIN: PinSlot = PinSlot {
    module: 0,
    vt_rva: VT_RVA,
    slot: VT_SLOT,
    target_rva: TARGET_RVA,
    what: "SYNTH_SLOT",
};
static SYNTH_BYTE_PINS: [&PinBytes; 1] = [&SYNTH_PROLOGUE_PIN];
static SYNTH_SLOT_PINS: [&PinSlot; 1] = [&SYNTH_SLOT_PIN];

fn put_u16(file: &mut [u8], offset: usize, value: u16) {
    file[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(file: &mut [u8], offset: usize, value: u32) {
    file[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(file: &mut [u8], offset: usize, value: u64) {
    file[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// The file offset of an RVA inside the single section.
fn file_offset(rva: u32) -> usize {
    usize::try_from(POINTER_TO_RAW_DATA + (rva - SECTION_RVA)).expect("the synthetic RVA fits")
}

/// An RVA as an index into a byte vector.
fn at(rva: u32) -> usize {
    usize::try_from(rva).expect("a u32 RVA fits a usize")
}

fn put_at(file: &mut [u8], rva: u32, bytes: &[u8]) {
    let offset = file_offset(rva);
    file[offset..offset + bytes.len()].copy_from_slice(bytes);
}

/// A synthetic module: its file bytes, the loader-style mapping of them, and the pin row that
/// describes the same build.
struct Fixture {
    file: Vec<u8>,
    mapped: Vec<u8>,
    identity: PinModule,
}

impl Fixture {
    fn new() -> Self {
        let mut file = vec![0u8; file_offset(SECTION_RVA + SECTION_VIRTUAL_SIZE)];
        file[..2].copy_from_slice(b"MZ");
        put_u32(&mut file, 0x3C, E_LFANEW as u32);
        put_u32(&mut file, PE_HDR, pe::PE_SIG);
        // IMAGE_FILE_HEADER (at PE_HDR + 4, after the signature): Machine amd64, one section.
        put_u16(&mut file, PE_HDR + 4, 0x8664);
        put_u16(&mut file, PE_HDR + 6, 1);
        put_u32(&mut file, PE_HDR + pe::PE_TIME_DATE_STAMP_OFF, TIME_STAMP);
        put_u16(&mut file, PE_HDR + pe::PE_SIZE_OF_OPTIONAL_HEADER_OFF, OPT_SIZE);
        put_u16(&mut file, PE_HDR + 22, 0x2022);
        // IMAGE_OPTIONAL_HEADER64.
        put_u16(&mut file, OPT_HDR, pe::PE32PLUS_MAGIC);
        put_u64(&mut file, OPT_HDR + pe::PE32PLUS_IMAGEBASE, BASE);
        put_u32(
            &mut file,
            OPT_HDR + pe::PE32PLUS_SIZEOFIMAGE,
            SIZE_OF_IMAGE,
        );
        put_u32(&mut file, OPT_HDR + pe::PE32PLUS_CHECKSUM, CHECK_SUM);
        put_u32(
            &mut file,
            OPT_HDR + pe::PE32PLUS_SIZEOFHEADERS,
            SIZE_OF_HEADERS,
        );
        put_u32(&mut file, OPT_HDR + pe::PE32PLUS_NRVA_OFF, 16);
        let debug_entry = OPT_HDR + pe::PE32PLUS_DATADIR_OFF + pe::PE_DATADIR_DEBUG * 8;
        put_u32(&mut file, debug_entry, DEBUG_RVA);
        put_u32(&mut file, debug_entry + 4, 28);
        // IMAGE_SECTION_HEADER: the one section.
        let section = OPT_HDR + usize::from(OPT_SIZE);
        file[section..section + 8].copy_from_slice(b".rdata\0\0");
        put_u32(
            &mut file,
            section + pe::PE_SECTION_VIRTUAL_SIZE_OFF,
            SECTION_VIRTUAL_SIZE,
        );
        put_u32(
            &mut file,
            section + pe::PE_SECTION_VIRTUAL_ADDRESS_OFF,
            SECTION_RVA,
        );
        put_u32(
            &mut file,
            section + pe::PE_SECTION_RAW_SIZE_OFF,
            SECTION_RAW_SIZE,
        );
        put_u32(
            &mut file,
            section + pe::PE_SECTION_RAW_PTR_OFF,
            POINTER_TO_RAW_DATA,
        );
        // The image contents: the pinned prologue, the pinned vtable word, the CodeView RSDS.
        put_at(&mut file, PROLOGUE_RVA, &PROLOGUE);
        put_u64(
            &mut file,
            file_offset(VT_RVA) + usize::from(VT_SLOT) * 8,
            BASE + u64::from(TARGET_RVA),
        );
        let debug = file_offset(DEBUG_RVA);
        put_u32(&mut file, debug + pe::PE_DBGDIR_TYPE_OFF, pe::PE_DBGDIR_TYPE_CODEVIEW);
        put_u32(&mut file, debug + pe::PE_DBGDIR_ADDRRAW_OFF, RSDS_RVA);
        put_at(&mut file, RSDS_RVA, b"RSDS");
        put_at(&mut file, RSDS_RVA + 4, &GUID);
        put_u32(&mut file, file_offset(RSDS_RVA) + pe::CV_RSDS_AGE_OFF, AGE);

        let mut fixture = Self {
            mapped: Vec::new(),
            identity: PinModule {
                name: "synth.dll",
                sha256: [0; 32],
                time_stamp: TIME_STAMP,
                size_of_image: SIZE_OF_IMAGE,
                check_sum: CHECK_SUM,
                rsds_guid: GUID,
                rsds_age: AGE,
            },
            file,
        };
        fixture.remap();
        fixture
    }

    /// Re-map the file after a mutation, and take the file's new digest (`remap` +
    /// [`rehash_identity`](Self::rehash_identity)): the mutation is then absorbed by the file
    /// hash and the check after it is the one under test.
    fn remap(&mut self) {
        self.remap_keep_digest();
        self.rehash_identity();
    }

    /// Re-map the file but keep the identity's digest, so a byte changed in a *mapped* part of
    /// the file still fails the file-hash check first.
    fn remap_keep_digest(&mut self) {
        let mapped = pe::map_image(&self.file).expect("the synthetic file always maps");
        assert_eq!(mapped.image_base, BASE, "the file's preferred base");
        self.mapped = mapped.bytes;
    }

    /// Re-hash the identity only: the file stays as it is, so a check other than the file hash
    /// is the one under test.
    fn rehash_identity(&mut self) {
        self.identity.sha256 = pe::sha256(&self.file);
    }

    fn image(&self) -> Image<'_> {
        Image::new(&self.mapped, BASE)
    }

    fn pins(&self) -> Pins<'_> {
        Pins {
            identity: &self.identity,
            bytes: &SYNTH_BYTE_PINS,
            slots: &SYNTH_SLOT_PINS,
        }
    }

    /// `verify_one` with the file the image came from and a pin call that succeeds.
    fn verify(&self) -> Result<(), Fail> {
        self.verify_with(FileHash::Digest(pe::sha256(&self.file)), true)
    }

    fn verify_with(&self, file: FileHash, pin_ok: bool) -> Result<(), Fail> {
        let image = self.image();
        verify_one(
            self.pins(),
            Some(&image),
            file,
            &mut Vec::new(),
            &std::sync::atomic::AtomicBool::new(false),
            |_rva| pin_ok,
        )
    }
}

/// The unmodified synthetic module verifies, and the pin gets the module's first byte-pin RVA
/// (the C computes exactly that at `mcp_bridge.c:1352-1356`).
#[test]
fn a_synthetic_image_that_matches_its_pins_verifies() {
    scratch_log();
    let fixture = Fixture::new();
    let pinned = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&pinned);
    let image = fixture.image();
    let result = verify_one(
        fixture.pins(),
        Some(&image),
        FileHash::Digest(pe::sha256(&fixture.file)),
        &mut Vec::new(),
        &std::sync::atomic::AtomicBool::new(false),
        {
            move |rva| {
                recorder.lock().expect("the recorder is not poisoned").push(rva);
                true
            }
        },
    );
    assert_eq!(result, Ok(()));
    assert_eq!(
        *pinned.lock().expect("the recorder is not poisoned"),
        vec![PROLOGUE_RVA],
        "the pinned address is the first byte pin's RVA"
    );
}

/// Every check `verify_one` runs, one single-field change at a time, each with the C's log line
/// and reason.
#[test]
fn each_single_field_change_fails_with_the_c_reason() {
    scratch_log();
    // TimeDateStamp: the file's, not the identity's.
    let mut fixture = Fixture::new();
    put_u32(&mut fixture.file, PE_HDR + pe::PE_TIME_DATE_STAMP_OFF, TIME_STAMP + 1);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::TimeDateStamp));
    assert_eq!(Fail::TimeDateStamp.line(), "verify: TimeDateStamp mismatch");

    // SizeOfImage.
    let mut fixture = Fixture::new();
    put_u32(&mut fixture.file, OPT_HDR + pe::PE32PLUS_SIZEOFIMAGE, SIZE_OF_IMAGE + 0x1000);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::SizeOfImage));
    assert_eq!(Fail::SizeOfImage.line(), "verify: SizeOfImage mismatch");

    // CheckSum.
    let mut fixture = Fixture::new();
    put_u32(&mut fixture.file, OPT_HDR + pe::PE32PLUS_CHECKSUM, CHECK_SUM ^ 1);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::CheckSum));
    assert_eq!(Fail::CheckSum.line(), "verify: CheckSum mismatch");

    // RSDS GUID.
    let mut fixture = Fixture::new();
    put_at(&mut fixture.file, RSDS_RVA + 4, &[0x00; 16]);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::Rsds));

    // RSDS age.
    let mut fixture = Fixture::new();
    put_u32(&mut fixture.file, file_offset(RSDS_RVA) + pe::CV_RSDS_AGE_OFF, AGE + 1);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::Rsds));
    assert_eq!(Fail::Rsds.line(), "verify: RSDS GUID/age mismatch");

    // No RSDS at all: the CodeView signature is gone.
    let mut fixture = Fixture::new();
    put_at(&mut fixture.file, RSDS_RVA, b"XXXX");
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::Rsds));

    // The file's SHA-256 only: a byte in the file that is in no mapped section. The identity
    // keeps the original digest (no `remap`), which is exactly what a swapped file looks like.
    let mut fixture = Fixture::new();
    fixture.file[GAP_BYTE] ^= 0xff;
    fixture.remap_keep_digest();
    assert_eq!(
        pe::map_image(&fixture.file).expect("still maps").bytes,
        fixture.mapped,
        "the gap byte is not in the image"
    );
    assert_eq!(fixture.verify(), Err(Fail::FileMismatch));
    assert_eq!(Fail::FileMismatch.line(), "verify: file SHA-256 mismatch");

    // One byte pin: the prologue changed, with the identity re-hashed so the file hash passes.
    let mut fixture = Fixture::new();
    let prologue = file_offset(PROLOGUE_RVA);
    fixture.file[prologue] ^= 0xff;
    fixture.remap();
    fixture.rehash_identity();
    assert_eq!(fixture.verify(), Err(Fail::BytePin("SYNTH_PROLOGUE")));
    assert_eq!(Fail::BytePin("SYNTH_PROLOGUE").line(), "verify: byte pin mismatch: ");
    assert_eq!(Fail::BytePin("SYNTH_PROLOGUE").detail(), Some("SYNTH_PROLOGUE"));

    // One slot pin: the vtable entry points elsewhere.
    let mut fixture = Fixture::new();
    put_u64(
        &mut fixture.file,
        file_offset(VT_RVA) + usize::from(VT_SLOT) * 8,
        BASE + u64::from(TARGET_RVA) + 1,
    );
    fixture.remap();
    fixture.rehash_identity();
    assert_eq!(fixture.verify(), Err(Fail::SlotPin("SYNTH_SLOT")));
    assert_eq!(Fail::SlotPin("SYNTH_SLOT").line(), "verify: vtable slot mismatch: ");

    // The module is gone (the C's `GetModuleHandleA` returning 0 after the poll saw it).
    let fixture = Fixture::new();
    let result = verify_one(
        fixture.pins(),
        None,
        FileHash::Digest(pe::sha256(&fixture.file)),
        &mut Vec::new(),
        &std::sync::atomic::AtomicBool::new(false),
        |_rva| true,
    );
    assert_eq!(result, Err(Fail::NotLoaded));
    assert_eq!(Fail::NotLoaded.line(), "verify: module not loaded any more");

    // PE parse: the DOS signature is gone from the *mapped image* (the loader hands over an
    // image, and that is what `pe_info` reads).
    let mut fixture = Fixture::new();
    fixture.mapped[0] = b'X';
    assert_eq!(fixture.verify(), Err(Fail::PeParse));
    assert_eq!(Fail::PeParse.line(), "verify: PE parse failed");

    // The two file-read failures.
    let fixture = Fixture::new();
    assert_eq!(fixture.verify_with(FileHash::NoName, true), Err(Fail::ModuleName));
    assert_eq!(Fail::ModuleName.line(), "verify: module file name failed");
    assert_eq!(fixture.verify_with(FileHash::Unreadable, true), Err(Fail::FileHash));
    assert_eq!(Fail::FileHash.line(), "verify: SHA-256 of file failed");

    // The pin call itself fails.
    assert_eq!(fixture.verify_with(FileHash::Digest(pe::sha256(&fixture.file)), false), Err(Fail::Pin));
    assert_eq!(Fail::Pin.line(), "verify: GetModuleHandleEx(PIN) failed");
}

/// The checks do not reorder: with several fields wrong the earliest check in the C's order
/// wins, and the file hash is checked before the pins.
#[test]
fn the_checks_run_in_the_c_order() {
    scratch_log();
    // Timestamp, checksum and GUID all wrong -> TimeDateStamp.
    let mut fixture = Fixture::new();
    put_u32(&mut fixture.file, PE_HDR + pe::PE_TIME_DATE_STAMP_OFF, TIME_STAMP + 1);
    put_u32(&mut fixture.file, OPT_HDR + pe::PE32PLUS_CHECKSUM, CHECK_SUM ^ 1);
    put_at(&mut fixture.file, RSDS_RVA + 4, &[0x11; 16]);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::TimeDateStamp));

    // SizeOfImage and CheckSum wrong -> SizeOfImage.
    let mut fixture = Fixture::new();
    put_u32(&mut fixture.file, OPT_HDR + pe::PE32PLUS_SIZEOFIMAGE, SIZE_OF_IMAGE + 0x1000);
    put_u32(&mut fixture.file, OPT_HDR + pe::PE32PLUS_CHECKSUM, CHECK_SUM ^ 1);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::SizeOfImage));

    // A changed file byte and a broken byte pin -> the file hash, which the C reads first
    // (`mcp_bridge.c:1336-1340` before `:1343-1354`).
    let mut fixture = Fixture::new();
    fixture.file[GAP_BYTE] ^= 0xff;
    fixture.file[file_offset(PROLOGUE_RVA)] ^= 0xff;
    fixture.remap_keep_digest();
    assert_eq!(fixture.verify(), Err(Fail::FileMismatch));

    // A broken byte pin and a broken slot pin -> the byte pins, in table order.
    let mut fixture = Fixture::new();
    fixture.file[file_offset(PROLOGUE_RVA)] ^= 0xff;
    put_u64(
        &mut fixture.file,
        file_offset(VT_RVA) + usize::from(VT_SLOT) * 8,
        BASE + u64::from(TARGET_RVA) + 1,
    );
    fixture.remap();
    fixture.rehash_identity();
    assert_eq!(fixture.verify(), Err(Fail::BytePin("SYNTH_PROLOGUE")));
}

/// The failure path writes the C's lines, in the C's order, to the bridge log.
#[test]
fn the_failure_lines_are_written_in_the_c_order() {
    let scratch = ScratchLog::new("failure-lines");
    let log_path = scratch.path.clone();

    let mut fixture = Fixture::new();
    put_u32(&mut fixture.file, PE_HDR + pe::PE_TIME_DATE_STAMP_OFF, TIME_STAMP + 1);
    fixture.remap();
    assert_eq!(fixture.verify(), Err(Fail::TimeDateStamp));

    let text = std::fs::read_to_string(&log_path).expect("the bridge log was written");
    let lines: Vec<&str> = text.lines().collect();
    let expected = [
        "[cs2-bridge none] verify: module ",
        "[cs2-bridge none] synth.dll",
        "[cs2-bridge none] verify: TimeDateStamp mismatch",
        "[cs2-bridge none] verify: FAIL -> fail closed (PrerequisiteFailed)",
    ];
    // In order, not necessarily adjacent: the other tests in this binary run in parallel and
    // their failing verifies append to the same log once the variable is set, so their lines can
    // interleave with these four.
    let mut want = expected.iter().peekable();
    for line in &lines {
        if want.peek() == Some(&line) {
            want.next();
        }
    }
    assert!(
        want.peek().is_none(),
        "the four C lines, in order, must appear in the log:\n{}",
        lines.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// The fake loader: the C's four loader calls, over a fabricated image that matches the pins.
// ---------------------------------------------------------------------------------------------

/// A fabricated image for `PIN_MODULES[index]`: the identity fields `pe_info` reads (written at
/// the winnt.h offsets the constants in `pe.rs` name), the RSDS record, every `PIN_BYTES` row's
/// prologue at its RVA and every `PIN_SLOTS` row's word at `base + target_rva`.
///
/// This is the image the real DLL *is*: the fake loader below is the Windows loader for a build
/// that matches the pins.
fn fabricated_image(index: usize, base: u64) -> Vec<u8> {
    let identity = &PIN_MODULES[index];
    let size = usize::try_from(identity.size_of_image).expect("a real SizeOfImage");
    let mut image = vec![0u8; size];
    image[..2].copy_from_slice(b"MZ");
    put_u32(&mut image, 0x3C, E_LFANEW as u32);
    put_u32(&mut image, PE_HDR, pe::PE_SIG);
    put_u16(&mut image, PE_HDR + 4, 0x8664);
    put_u16(&mut image, PE_HDR + 6, 1);
    put_u16(&mut image, OPT_HDR, pe::PE32PLUS_MAGIC);
    put_u32(&mut image, PE_HDR + pe::PE_TIME_DATE_STAMP_OFF, identity.time_stamp);
    put_u32(&mut image, OPT_HDR + pe::PE32PLUS_SIZEOFIMAGE, identity.size_of_image);
    put_u32(&mut image, OPT_HDR + pe::PE32PLUS_CHECKSUM, identity.check_sum);
    put_u32(&mut image, OPT_HDR + pe::PE32PLUS_NRVA_OFF, 16);
    let debug_entry = OPT_HDR + pe::PE32PLUS_DATADIR_OFF + pe::PE_DATADIR_DEBUG * 8;
    put_u32(&mut image, debug_entry, DEBUG_RVA);
    put_u32(&mut image, debug_entry + 4, 28);
    put_u32(
        &mut image,
        at(DEBUG_RVA) + pe::PE_DBGDIR_TYPE_OFF,
        pe::PE_DBGDIR_TYPE_CODEVIEW,
    );
    put_u32(
        &mut image,
        at(DEBUG_RVA) + pe::PE_DBGDIR_ADDRRAW_OFF,
        RSDS_RVA,
    );
    image[at(RSDS_RVA)..at(RSDS_RVA) + 4].copy_from_slice(b"RSDS");
    image[at(RSDS_RVA) + 4..at(RSDS_RVA) + 20].copy_from_slice(&identity.rsds_guid);
    put_u32(
        &mut image,
        at(RSDS_RVA) + pe::CV_RSDS_AGE_OFF,
        identity.rsds_age,
    );

    for pin in PIN_BYTES.iter().filter(|pin| usize::from(pin.module) == index) {
        let rva = usize::try_from(pin.rva).expect("a u32 RVA");
        let len = usize::from(pin.len);
        assert!(
            rva >= 0x400 && rva + len <= size,
            "{}.{} does not fit the fabricated image",
            identity.name,
            pin.what
        );
        image[rva..rva + len].copy_from_slice(&pin.bytes[..len]);
    }
    for pin in PIN_SLOTS.iter().filter(|pin| usize::from(pin.module) == index) {
        let word = usize::try_from(pin.vt_rva).expect("a u32 RVA")
            + usize::from(pin.slot) * 8;
        assert!(word + 8 <= size, "{}.{} does not fit", identity.name, pin.what);
        image[word..word + 8]
            .copy_from_slice(&(base + u64::from(pin.target_rva)).to_le_bytes());
    }
    image
}

/// The loader the C calls, for a build that matches the pins.
///
/// * `module_base` is `GetModuleHandleA` (`mcp_bridge.c:1310`): `Some(base)` for the
///   modules in `loaded`, `None` otherwise.
/// * `loaded`/`host_path` are the Metamod-proof pair the Rust build uses instead of the base
///   name: `K32EnumProcessModules` + `GetModuleFileNameW` for the process's images and
///   `GetModuleFileNameW(NULL)` for the host executable. Empty `instances` keeps the single
///   pathless image per name (a process with one image per module).
/// * `image` is reading the mapped image at that base; it asserts it was handed the base
///   `module_base` returned and that the name is a pin, and it records what it was asked for.
/// * `file_hash` is `GetModuleFileNameA` + `sha256_file` (`:1330-1340`): the file of the loaded
///   build hashes to its `PIN_MODULES` row, and for `block_on` it waits on the test's channel
///   first (to hold a verifier inside a check).
/// * `pin` is `GetModuleHandleExW(MODULE_PIN_FLAGS, base + rva, &module)` (`:1358`); it asserts
///   the RVA is the module's first byte pin, which is the address the C pins by.
#[derive(Clone, Default)]
struct Recorder {
    /// The (name, base) pairs `image` received, in call order.
    reads: Arc<Mutex<Vec<(String, u64)>>>,
    /// The (name, base, rva) triples `pin` received.
    pins: Arc<Mutex<Vec<(String, u64, u32)>>>,
}

struct FakePlatform {
    loaded: Vec<&'static str>,
    block_on: Option<&'static str>,
    release: Mutex<Option<Receiver<()>>>,
    recorder: Recorder,
    /// Modules whose fabricated image gets one pinned prologue byte flipped (a real mismatch).
    broken: Vec<&'static str>,
    /// The images the loader reports for the `loaded` question, in loader order (a Metamod host
    /// maps two `server.dll`s). Empty: the default, one pathless image per `loaded` name.
    instances: Vec<LoadedModule>,
    /// The host executable's path (`GetModuleFileNameW(NULL)`).
    host: Option<&'static str>,
}

impl FakePlatform {
    fn loaded(names: &[&'static str]) -> Self {
        Self {
            loaded: names.to_vec(),
            block_on: None,
            release: Mutex::new(None),
            recorder: Recorder::default(),
            broken: Vec::new(),
            instances: Vec::new(),
            host: None,
        }
    }

    /// A loader that reports `instances` for the images named in `loaded`, and `host` for the
    /// host executable - the shape a Metamod host has (two images for one base name).
    fn with_instances(
        names: &[&'static str],
        instances: Vec<LoadedModule>,
        host: &'static str,
    ) -> Self {
        Self {
            instances,
            host: Some(host),
            ..Self::loaded(names)
        }
    }

    fn blocking(names: &[&'static str], block_on: &'static str, release: Receiver<()>) -> Self {
        let mut platform = Self::loaded(names);
        platform.block_on = Some(block_on);
        platform.release = Mutex::new(Some(release));
        platform
    }

    fn index_of(name: &str) -> usize {
        PIN_MODULES
            .iter()
            .position(|pin| pin.name == name)
            .unwrap_or_else(|| panic!("{name} is not a pinned module"))
    }

    fn base_of(index: usize) -> u64 {
        // The loader may place each module anywhere; distinct bases make a wrong base visible.
        0x1_8000_0000 + (index as u64) * 0x1000_0000
    }
}

impl Platform for FakePlatform {
    fn module_base(&self, name: &str) -> Option<u64> {
        if !self.loaded.contains(&name) {
            return None;
        }
        Some(Self::base_of(Self::index_of(name)))
    }

    /// `K32EnumProcessModules` + `GetModuleFileNameW`: the images the loader reports under
    /// `name`, in loader order. Empty `instances` keeps the default (one pathless image).
    fn loaded(&self, name: &str) -> Vec<LoadedModule> {
        if self.instances.is_empty() {
            return self
                .module_base(name)
                .map(|base| LoadedModule {
                    base,
                    path: String::new(),
                })
                .into_iter()
                .collect();
        }
        self.instances
            .iter()
            .filter(|instance| {
                instance
                    .path
                    .rsplit(['\\', '/'])
                    .next()
                    .is_some_and(|file| file.eq_ignore_ascii_case(name))
            })
            .cloned()
            .collect()
    }

    fn host_path(&self) -> Option<String> {
        self.host.map(str::to_owned)
    }

    fn image(&self, name: &str, base: u64) -> Option<Vec<u8>> {
        let index = Self::index_of(name);
        assert_eq!(base, Self::base_of(index), "the base module_base returned");
        self.recorder
            .reads
            .lock()
            .expect("the reads are not poisoned")
            .push((name.to_owned(), base));
        let mut image = fabricated_image(index, base);
        if self.broken.contains(&name) {
            let pin = PIN_BYTES
                .iter()
                .find(|pin| usize::from(pin.module) == index)
                .expect("every pinned module has a byte pin");
            image[usize::try_from(pin.rva).expect("a u32 RVA")] ^= 0xff;
        }
        Some(image)
    }

    fn file_hash(&self, name: &str, base: u64) -> FileHash {
        let index = Self::index_of(name);
        assert_eq!(base, Self::base_of(index), "the base module_base returned");
        if self.block_on == Some(name) {
            let release = self
                .release
                .lock()
                .expect("the release channel is not poisoned")
                .take()
                .expect("one blocked verification");
            release
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|error| match error {
                    RecvTimeoutError::Timeout => panic!("the test never released {name}"),
                    RecvTimeoutError::Disconnected => panic!("the test dropped {name}'s sender"),
                });
        }
        FileHash::Digest(PIN_MODULES[index].sha256)
    }

    fn pin(&self, name: &str, base: u64, rva: u32) -> bool {
        let index = Self::index_of(name);
        assert_eq!(base, Self::base_of(index), "the base module_base returned");
        let first = PIN_BYTES
            .iter()
            .find(|pin| usize::from(pin.module) == index)
            .expect("every pinned module has a byte pin");
        assert_eq!(rva, first.rva, "the C pins by the first byte pin's RVA");
        self.recorder
            .pins
            .lock()
            .expect("the pin calls are not poisoned")
            .push((name.to_owned(), base, rva));
        true
    }
}

/// The Metamod host's real layout: `<game>\bin\win64\cs2.exe` is the host, Metamod's loader stub
/// and the game's own `server.dll` are both mapped (`server.dll` in `csgo\bin\win64`).
const HOST_EXE: &str = r"Z:\cs2\game\bin\win64\cs2.exe";
const METAMOD_STUB: &str = r"Z:\cs2\game\metamod\bin\win64\server.dll";
const GAME_SERVER: &str = r"Z:\cs2\game\csgo\bin\win64\server.dll";

/// A Metamod host maps two images named `server.dll`: the loader stub first (so the C's
/// `GetModuleHandleA` answered it and the TimeDateStamp check failed closed) and the game's own
/// binary. The walk must verify the game's image - the stub is never read.
#[test]
fn the_game_image_is_verified_over_metamods_loader_stub() {
    scratch_log();
    let index = FakePlatform::index_of("server.dll");
    let game_base = FakePlatform::base_of(index);
    let platform = FakePlatform::with_instances(
        &["server.dll"],
        vec![
            LoadedModule {
                base: 0x1000_0000,
                path: METAMOD_STUB.to_string(),
            },
            LoadedModule {
                base: game_base,
                path: GAME_SERVER.to_string(),
            },
        ],
        HOST_EXE,
    );
    let recorder = platform.recorder.clone();
    let modules = Modules::new(platform);

    modules.verify_all();

    assert_eq!(
        modules.state(index as u8),
        Some(State::Verified),
        "the game's server.dll is the pinned one"
    );
    assert_eq!(modules.base(index as u8), game_base);
    let reads = recorder.reads.lock().expect("not poisoned").clone();
    assert_eq!(
        reads,
        vec![("server.dll".to_string(), game_base)],
        "only the game's image was read, never the stub"
    );
}

/// Only Metamod's loader stub is mapped: the module stays `not_loaded` (it is not verified
/// against the stub's header, which would fail closed for the wrong reason and never retry), and
/// nothing is read from the stub. The one refusal line is a library test
/// (`modules::tests::a_stub_only_module_stays_not_loaded_and_says_so_once`), where the log file
/// belongs to that case alone.
#[test]
fn a_lone_loader_stub_leaves_the_module_not_loaded() {
    scratch_log();
    let index = FakePlatform::index_of("server.dll");
    let platform = FakePlatform::with_instances(
        &["server.dll"],
        vec![LoadedModule {
            base: 0x1000_0000,
            path: METAMOD_STUB.to_string(),
        }],
        HOST_EXE,
    );
    let recorder = platform.recorder.clone();
    let modules = Modules::new(platform);

    modules.verify_all();
    modules.verify_all();

    assert_eq!(
        modules.state(index as u8),
        Some(State::NotLoaded),
        "the stub is never verified, so the module is still 'not loaded'"
    );
    assert_eq!(modules.base(index as u8), 0, "and never based on the stub");
    assert!(
        recorder.reads.lock().expect("not poisoned").is_empty(),
        "nothing was read from the stub, not even its header"
    );
}

/// Run one module alone through the real state machine: it verifies, its own `NEED_*` bit is in
/// the mask, nothing else is, and the caps revision counted it.
#[test]
fn every_module_verifies_and_contributes_its_own_need_bit() {
    scratch_log();
    for (index, module) in PIN_MODULES.iter().enumerate() {
        let name = module.name;
        let platform = FakePlatform::loaded(&[name]);
        let modules = Modules::new(platform);
        assert_eq!(modules.verified_mask(), 0, "{name}: nothing verified yet");
        assert_eq!(modules.state(index as u8), Some(State::NotLoaded));

        modules.verify_all();

        assert_eq!(
            modules.state(index as u8),
            Some(State::Verified),
            "{name} must verify"
        );
        assert!(
            modules.is_verified(index as u8),
            "{name} is verified and has a base"
        );
        assert_eq!(
            modules.verified_mask(),
            need_bit(index as u8),
            "{name}: only its own bit"
        );
        assert_eq!(modules.caps_revision(), 2, "{name}: one bump for one module");
        assert!(!modules.verifier_running());
    }
}

/// A module that is loaded but does not match never verifies: it stops in `mismatch`, holds no
/// base, contributes no bit, and a later run does not retry it.
#[test]
fn a_mismatch_module_stays_fail_closed() {
    scratch_log();
    let index = PIN_TF2 as usize;
    let name = PIN_MODULES[index].name;
    let mut platform = FakePlatform::loaded(&[name]);
    platform.broken = vec![name];
    let modules = Modules::new(platform);

    modules.verify_all();

    assert_eq!(modules.state(PIN_TF2), Some(State::Mismatch));
    assert!(!modules.is_verified(PIN_TF2));
    assert_eq!(modules.base(PIN_TF2), 0);
    assert_eq!(modules.verified_mask(), 0);
    assert_eq!(modules.state_name(PIN_TF2), "mismatch");
    assert_eq!(modules.caps_revision(), 2, "the failed check still bumped");

    // Not loaded again: the state is no longer `not_loaded`, so the walk skips it.
    modules.verify_all();
    assert_eq!(modules.state(PIN_TF2), Some(State::Mismatch));
    assert_eq!(modules.caps_revision(), 2, "no retry, no second bump");
}

/// A module that is not loaded is left alone - and one that is loaded alongside it still
/// verifies.
#[test]
fn only_loaded_modules_are_verified() {
    scratch_log();
    let name = PIN_MODULES[PIN_TIER0 as usize].name;
    let modules = Modules::new(FakePlatform::loaded(&[name]));
    modules.verify_all();

    assert_eq!(modules.state(PIN_TIER0), Some(State::Verified));
    assert_eq!(modules.state(PIN_TF2), Some(State::NotLoaded));
    assert_eq!(modules.state(0), Some(State::NotLoaded));
    assert_eq!(modules.verified_mask(), need_bit(PIN_TIER0));
    assert_eq!(modules.caps_revision(), 2);
}

/// The poll's throttle: 31 calls do nothing, the 32nd starts the verifier.
#[test]
fn poll_throttles_to_one_tick_in_32() {
    scratch_log();
    let name = PIN_MODULES[PIN_TIER0 as usize].name;
    let modules = Modules::new(FakePlatform::loaded(&[name]));

    for call in 1..32 {
        assert_eq!(
            modules.poll(),
            cs2_bridge::modules::Poll::Throttled,
            "call {call} is not a poll tick"
        );
        assert!(!modules.verifier_running(), "call {call} started nothing");
        assert_eq!(modules.state(PIN_TIER0), Some(State::NotLoaded));
    }
    assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Started);
    assert!(wait_idle(&modules, Duration::from_secs(10)));
    assert_eq!(modules.state(PIN_TIER0), Some(State::Verified));
}

/// With no module loaded, the 32nd call is a tick but starts nothing.
#[test]
fn a_poll_tick_with_nothing_loaded_starts_nothing() {
    scratch_log();
    let modules = Modules::new(FakePlatform::loaded(&[]));
    for _ in 1..32 {
        assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Throttled);
    }
    assert_eq!(modules.poll(), cs2_bridge::modules::Poll::NothingToDo);
    assert!(!modules.verifier_running());
    assert_eq!(modules.caps_revision(), 1);
}

/// One verifier at a time: while a verifier is held inside a check, a tick that has a new module
/// to check gets `AlreadyVerifying` rather than a second thread, and both modules end verified.
#[test]
fn a_tick_does_not_start_a_second_verifier() {
    scratch_log();
    let blocked = PIN_MODULES[PIN_TIER0 as usize].name;
    let waiting = PIN_MODULES[8].name; // schemasystem: after tier0 in ORDER
    let (release, held) = std::sync::mpsc::channel();
    let modules = Modules::new(FakePlatform::blocking(
        &[blocked, waiting],
        blocked,
        held,
    ));

    for _ in 1..32 {
        assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Throttled);
    }
    assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Started);
    // The verifier is inside tier0's file hash: the state must have gone to `verifying` and
    // stay there until the test releases it.
    assert!(modules.verifier_running());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while modules.state(PIN_TIER0) == Some(State::NotLoaded) {
        assert!(
            std::time::Instant::now() < deadline,
            "the verifier never marked tier0 as verifying"
        );
        std::thread::yield_now();
    }
    assert_eq!(modules.state(PIN_TIER0), Some(State::Verifying));
    assert!(!modules.is_verified(PIN_TIER0), "not verified while held");

    // Ticks up to the next multiple of 32: the first one has schemasystem still `not_loaded`
    // and loaded, so it tries - and finds the verifier running.
    for _ in 33..64 {
        assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Throttled);
    }
    assert_eq!(
        modules.poll(),
        cs2_bridge::modules::Poll::AlreadyVerifying
    );

    release.send(()).expect("the verifier is still waiting");
    assert!(wait_idle(&modules, Duration::from_secs(10)));
    assert_eq!(modules.state(PIN_TIER0), Some(State::Verified));
    assert_eq!(modules.state(8), Some(State::Verified));
    assert_eq!(modules.verified_mask(), need_bit(PIN_TIER0) | need_bit(8));
    assert!(!modules.verifier_running(), "the flag was cleared at the end");
}

/// `wait_verifier` blocks on the verifier's own signal, not a timer: a verifier held inside a
/// check is reported still running at a deadline, and the wait returns as soon as the verifier
/// lets go (the condvar `RunningGuard` notifies). This is the wait `plugin::unload` uses before
/// it forgets the `IKHook` the verifier's identity check holds.
#[test]
fn wait_verifier_waits_for_the_running_verifier() {
    scratch_log();
    let blocked = PIN_MODULES[PIN_TIER0 as usize].name;
    let (release, held) = std::sync::mpsc::channel();
    let modules = Modules::new(FakePlatform::blocking(&[blocked], blocked, held));

    for _ in 1..32 {
        assert_eq!(
            modules.poll(),
            cs2_bridge::modules::Poll::Throttled
        );
    }
    assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Started);
    // The verifier is inside tier0's file hash: `verifying`, not done.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while modules.state(PIN_TIER0) == Some(State::NotLoaded) {
        assert!(
            std::time::Instant::now() < deadline,
            "the verifier never marked tier0 as verifying"
        );
        std::thread::yield_now();
    }
    assert!(modules.verifier_running());
    // A zero deadline asks once: still running, nothing was waited for.
    assert!(!modules.wait_verifier(Duration::ZERO));
    // A real deadline times out on the held verifier instead of waiting for ever.
    assert!(
        !modules.wait_verifier(Duration::from_millis(50)),
        "the held verifier outlives the deadline"
    );

    release.send(()).expect("the verifier is still waiting");
    // The wait wakes on the signal and sees the verifier idle, long before its own deadline.
    let started = std::time::Instant::now();
    assert!(
        modules.wait_verifier(Duration::from_secs(10)),
        "the released verifier goes idle"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait returned on the signal, not on its deadline"
    );
    assert_eq!(modules.state(PIN_TIER0), Some(State::Verified));
    assert!(!modules.verifier_running());
}

/// `Modules` clones share one state (the C's globals), and the fake loader saw exactly the
/// arguments the C passes: one image read per module at the base `GetModuleHandleA` returned,
/// and one pin at the first byte pin's RVA.
#[test]
fn the_adapter_is_called_the_way_the_c_calls_it() {
    scratch_log();
    let name = PIN_MODULES[PIN_TF2 as usize].name;
    let platform = FakePlatform::loaded(&[name]);
    let recorder = platform.recorder.clone();
    let modules = Modules::new(platform);
    let clone = modules.clone();
    modules.verify_all();

    assert_eq!(clone.state(PIN_TF2), Some(State::Verified));
    assert_eq!(clone.verified_mask(), modules.verified_mask());
    assert_eq!(clone.caps_revision(), modules.caps_revision());
    assert!(!modules.verifier_running());

    let index = FakePlatform::index_of(name);
    let base = FakePlatform::base_of(index);
    assert_eq!(
        *recorder.reads.lock().expect("not poisoned"),
        vec![(name.to_owned(), base)]
    );
    let first = PIN_BYTES
        .iter()
        .find(|pin| usize::from(pin.module) == index)
        .expect("toolframework2 has a byte pin");
    assert_eq!(
        *recorder.pins.lock().expect("not poisoned"),
        vec![(name.to_owned(), base, first.rva)]
    );
}

/// `pins_for` gives exactly the generated rows of one module, in table order.
#[test]
fn pins_for_filters_by_module_in_table_order() {
    scratch_log();
    for index in 0..PIN_MODULES.len() {
        let module = index as u8;
        let (bytes, slots) = pins_for(module);
        let expected_bytes: Vec<&str> = PIN_BYTES
            .iter()
            .filter(|pin| pin.module == module)
            .map(|pin| pin.what)
            .collect();
        let expected_slots: Vec<&str> = PIN_SLOTS
            .iter()
            .filter(|pin| pin.module == module)
            .map(|pin| pin.what)
            .collect();
        assert_eq!(
            bytes.iter().map(|pin| pin.what).collect::<Vec<_>>(),
            expected_bytes,
            "module {module} byte pins"
        );
        assert_eq!(
            slots.iter().map(|pin| pin.what).collect::<Vec<_>>(),
            expected_slots,
            "module {module} slot pins"
        );
    }
}

/// Point the bridge log at `target/scratch/` for this test binary, and answer the path.
///
/// An integration test links the library **without** `cfg(test)`, so `log::line`'s default
/// path would be the user's real `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log` (`src/log.rs`). A
/// `cargo test` must never append to that file, so every case that can log points
/// `CS2_BRIDGE_LOG` here first. Process-wide and idempotent: the first call sets the variable
/// and every later call answers the same path (a case that logs to its own file restores it
/// with this).
fn scratch_log() -> std::path::PathBuf {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&dir).expect("target/scratch is writable");
        let path = dir.join("cs2-bridge-modules-test.log");
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        path
    })
    .clone()
}

// ---------------------------------------------------------------------------------------------
// The verifier's stop and join: `plugin::unload`'s sequence (review round 2, item 5)
// ---------------------------------------------------------------------------------------------

/// Points the bridge's log at `target/scratch/<name>.log` for the case (never the user's
/// `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log`).
///
/// `CS2_BRIDGE_LOG` is process-wide, so every case that repoints it holds [`LOG_ENV`] for its
/// whole life: two of them in parallel would restore each other's value and lose lines.
struct ScratchLog {
    path: PathBuf,
    previous: Option<String>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

/// Serialises the cases that repoint `CS2_BRIDGE_LOG` ([`ScratchLog`]). The other cases only
/// call [`scratch_log`], which sets the variable once and never changes it again.
static LOG_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl ScratchLog {
    fn new(name: &str) -> Self {
        let guard = LOG_ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Pin the binary's log first so `previous` is it (and a first-time `scratch_log` cannot
        // overwrite this case's value later).
        scratch_log();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&dir).expect("target/scratch exists");
        let path = dir.join(format!("modules-{name}.log"));
        let _ = std::fs::remove_file(&path);
        let previous = std::env::var("CS2_BRIDGE_LOG").ok();
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        Self { path, previous, _guard: guard }
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }
}

impl Drop for ScratchLog {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("CS2_BRIDGE_LOG", value),
            None => std::env::remove_var("CS2_BRIDGE_LOG"),
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A verifier held inside a check: `begin_stop` makes it stop before its next call into KHook
/// (`Fail::Stopped`), its module stays `not_loaded`, and `join_verifier` takes the thread out of
/// the image - the sequence `plugin::unload` runs before Metamod `dlclose`s us.
#[test]
fn begin_stop_makes_a_running_verifier_stop_and_join_verifier_takes_it_out() {
    // Pins the binary's log first, so this case's `ScratchLog::drop` restores it instead of
    // removing the variable (a later case's lines would then reach the user's real log).
    let _binary_log = scratch_log();
    let _scratch = ScratchLog::new("begin-stop");
    let blocked = PIN_MODULES[PIN_TIER0 as usize].name;
    let (release, held) = std::sync::mpsc::channel();
    let modules = Modules::new(FakePlatform::blocking(&[blocked], blocked, held));

    for _ in 1..32 {
        assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Throttled);
    }
    assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Started);
    // Wait until the verifier is inside tier0's file hash.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while modules.state(PIN_TIER0) == Some(State::NotLoaded) {
        assert!(
            std::time::Instant::now() < deadline,
            "the verifier never marked tier0 as verifying"
        );
        std::thread::yield_now();
    }
    let revision = modules.caps_revision();

    // `plugin::unload`'s first step: ask the verifier to stop before its next KHook question.
    modules.begin_stop();
    assert!(modules.verifier_stopping());
    release.send(()).expect("the verifier is still waiting");

    // The wait proves the body finished; the join takes the thread out of the image.
    assert!(
        modules.wait_verifier(Duration::from_secs(10)),
        "the stopped verifier goes idle"
    );
    assert!(
        modules.join_verifier(),
        "the verifier's handle was joined, not detached"
    );
    assert!(!modules.verifier_running());
    assert!(
        modules.join_verifier(),
        "no handle is left: `None` answers true, so nothing was detached"
    );
    // The module was stopped before any pin could ask KHook: it stays `not_loaded` for a tick
    // after a reload, and nothing was verified or counted.
    assert_eq!(modules.state(PIN_TIER0), Some(State::NotLoaded));
    assert_eq!(modules.caps_revision(), revision, "nothing was verified");
    assert!(
        _scratch.text().contains("stopped"),
        "the stop is in the log: {}",
        _scratch.text()
    );
}

/// An mm-only image's tick (the GameFrame callback) is what starts the verifier, and the unload
/// sequence's wait + join removes the thread from the image. `poll` is the tick's starter; the
/// steps after it are `plugin::unload`'s, in its order.
#[test]
fn the_ticks_verifier_is_joined_by_the_unload_sequence() {
    let _binary_log = scratch_log();
    let blocked = PIN_MODULES[PIN_TIER0 as usize].name;
    let (release, held) = std::sync::mpsc::channel();
    let modules = Modules::new(FakePlatform::blocking(&[blocked], blocked, held));

    // One tick in 32 is a poll tick (`modules::poll`); this is the GameFrame tick's only starter.
    for _ in 1..32 {
        assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Throttled);
    }
    assert_eq!(modules.poll(), cs2_bridge::modules::Poll::Started);
    assert!(modules.verifier_running(), "the tick started a verifier");

    // Unload: begin_stop -> wait_verifier -> join_verifier.
    modules.begin_stop();
    release.send(()).expect("the verifier is still waiting");
    assert!(modules.wait_verifier(Duration::from_secs(10)));
    assert!(modules.join_verifier(), "the tick's verifier is joined");
    assert!(!modules.verifier_running());

    // A later tick verifies the module again after the reload, with the flag cleared.
    modules.resume_verify();
    modules.verify_all();
    assert_eq!(modules.state(PIN_TIER0), Some(State::Verified));
}
