//! The KHook-aware half of the module identity gate: [`verify_one`] and [`obj_is_in`] when
//! KHook has replaced a vtable entry or an inline function's prologue.
//!
//! This lives in its own test binary because it installs a mock `KHookApi` in the process-wide
//! slot (`s2sdk::hook`): the other module-gate tests must see exactly the no-API behaviour, so
//! they must not share a binary with a case that installs an API.
//!
//! Every mock answer cites the KHook source it stands in for:
//!
//! * `FindOriginalVirtual` answers the pre-hook entry of `vtable[index]`, or the slot's current
//!   entry when KHook never hooked it (`third_party/khook/src/detour.cpp:1907-1915`); the hook
//!   itself wrote the class's shared table in place and never restores it (`:96-103`).
//! * `FindOriginal` answers the pre-hook function, or the function itself when it is not
//!   hooked (`detour.cpp:1897-1905`).
//!
//! The synthetic image is the same shape `tests/modules.rs` builds: a real PE32+ file mapped
//! the way the loader maps it, with one prologue pin and one vtable-slot pin the gate can check.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cs2_bridge::generated::pins::{PinBytes, PinModule, PinSlot};
use cs2_bridge::modules::{slot_matches, verify_one, Fail, FileHash, Image, Pins, Relaxation};
use cs2_bridge::ops::ident::obj_is_in_relaxed;
use cs2_bridge::pe;

// ---------------------------------------------------------------------------------------------
// The synthetic PE32+ image (the same layout `tests/modules.rs` uses).
// ---------------------------------------------------------------------------------------------

const E_LFANEW: usize = 0x40;
const PE_HDR: usize = E_LFANEW;
const OPT_HDR: usize = PE_HDR + 24;
const OPT_SIZE: u16 = 0xF0;
const SIZE_OF_HEADERS: u32 = 0x200;
const SECTION_RVA: u32 = 0x1000;
const SECTION_VIRTUAL_SIZE: u32 = 0x1000;
const SECTION_RAW_SIZE: u32 = 0x1000;
const POINTER_TO_RAW_DATA: u32 = 0x400;
const SIZE_OF_IMAGE: u32 = 0x2000;
const PROLOGUE_RVA: u32 = 0x1100;
const PROLOGUE: [u8; 6] = [0x48, 0x89, 0x5c, 0x24, 0x08, 0x57];
const VT_RVA: u32 = 0x1200;
const VT_SLOT: u16 = 3;
const TARGET_RVA: u32 = 0x1300;
const DEBUG_RVA: u32 = 0x1800;
const RSDS_RVA: u32 = 0x1900;
const BASE: u64 = 0x1_8000_0000;
const TIME_STAMP: u32 = 0x6ac8_001f;
const CHECK_SUM: u32 = 0x025d_45b7;
const GUID: [u8; 16] = [
    0xd4, 0x85, 0xa5, 0xa2, 0xc8, 0x06, 0xa5, 0x47, 0x82, 0x8a, 0xdb, 0xef, 0x47, 0x76, 0xae,
    0xdd,
];
const AGE: u32 = 7;
/// The address KHook leaves in a hooked slot: not a function in the image, which is what makes
/// the exact compare fail and the KHook question necessary.
const FAKE_JIT: u64 = 0x7f00_dead_beef_0000;

static PROLOGUE_PIN: PinBytes = PinBytes {
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
static SLOT_PIN: PinSlot = PinSlot {
    module: 0,
    vt_rva: VT_RVA,
    slot: VT_SLOT,
    target_rva: TARGET_RVA,
    what: "SYNTH_SLOT",
};
static BYTE_PINS: [&PinBytes; 1] = [&PROLOGUE_PIN];
static SLOT_PINS: [&PinSlot; 1] = [&SLOT_PIN];

fn put_u16(file: &mut [u8], offset: usize, value: u16) {
    file[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(file: &mut [u8], offset: usize, value: u32) {
    file[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(file: &mut [u8], offset: usize, value: u64) {
    file[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn file_offset(rva: u32) -> usize {
    usize::try_from(POINTER_TO_RAW_DATA + (rva - SECTION_RVA)).expect("the synthetic RVA fits")
}

fn at(rva: u32) -> usize {
    usize::try_from(rva).expect("a u32 RVA fits a usize")
}

fn put_at(file: &mut [u8], rva: u32, bytes: &[u8]) {
    let offset = file_offset(rva);
    file[offset..offset + bytes.len()].copy_from_slice(bytes);
}

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
        put_u16(&mut file, PE_HDR + 4, 0x8664);
        put_u16(&mut file, PE_HDR + 6, 1);
        put_u32(&mut file, PE_HDR + pe::PE_TIME_DATE_STAMP_OFF, TIME_STAMP);
        put_u16(&mut file, PE_HDR + pe::PE_SIZE_OF_OPTIONAL_HEADER_OFF, OPT_SIZE);
        put_u16(&mut file, PE_HDR + 22, 0x2022);
        put_u16(&mut file, OPT_HDR, pe::PE32PLUS_MAGIC);
        put_u64(&mut file, OPT_HDR + pe::PE32PLUS_IMAGEBASE, BASE);
        put_u32(&mut file, OPT_HDR + pe::PE32PLUS_SIZEOFIMAGE, SIZE_OF_IMAGE);
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
        put_u32(&mut file, section + pe::PE_SECTION_RAW_SIZE_OFF, SECTION_RAW_SIZE);
        put_u32(&mut file, section + pe::PE_SECTION_RAW_PTR_OFF, POINTER_TO_RAW_DATA);
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

    fn remap(&mut self) {
        let mapped = pe::map_image(&self.file).expect("the synthetic file always maps");
        assert_eq!(mapped.image_base, BASE, "the file's preferred base");
        self.mapped = mapped.bytes;
        self.identity.sha256 = pe::sha256(&self.file);
    }

    /// Overwrite the mapped image's pinned vtable slot by a word the pins do not hold (what a
    /// KHook `SetupVirtual` leaves there, `src/detour.hpp:96-104`).
    fn hook_slot(&mut self) {
        self.write_word(VT_RVA, VT_SLOT, FAKE_JIT);
    }

    /// Overwrite the mapped image's pinned vtable slot with `word`.
    fn write_word(&mut self, vt_rva: u32, slot: u16, word: u64) {
        let offset = at(vt_rva) + usize::from(slot) * 8;
        put_u64(&mut self.mapped, offset, word);
    }

    /// Overwrite the pinned prologue (what a KHook `SetupAddress` inline detour leaves there,
    /// `src/detour.hpp:82-94`).
    fn hook_prologue(&mut self) {
        let offset = at(PROLOGUE_RVA);
        self.mapped[offset..offset + PROLOGUE.len()].fill(0xE9);
    }

    fn image(&self) -> Image<'_> {
        Image::new(&self.mapped, BASE)
    }

    fn pins(&self) -> Pins<'_> {
        Pins {
            identity: &self.identity,
            bytes: &BYTE_PINS,
            slots: &SLOT_PINS,
        }
    }

    /// [`verify_one`] over the current mapped image, with the fixture's own file hash.
    fn verify(&self, relaxed: &mut Vec<Relaxation>) -> Result<(), Fail> {
        let image = self.image();
        verify_one(
            self.pins(),
            Some(&image),
            FileHash::Digest(pe::sha256(&self.file)),
            relaxed,
            &std::sync::atomic::AtomicBool::new(false),
            |_rva| true,
        )
    }
}

// ---------------------------------------------------------------------------------------------
// The mock `KHookApi`: only the two FindOriginal answers are modelled (see the module docs).
// ---------------------------------------------------------------------------------------------

use core::ffi::c_void;
use s2sdk::hook::{Action, HookId, KHookApi};

/// The `FindOriginal` answers: `0` means "answer the input unchanged", the real KHook's answer
/// for a target it never hooked (`detour.cpp:1897-1915`). Serialised by [`TEST_LOCK`]: the API
/// slot is process-wide, like `s2sdk::hook` says.
static INLINE: AtomicUsize = AtomicUsize::new(0);
static VIRTUAL: AtomicUsize = AtomicUsize::new(0);

/// The address-keyed answer, the shape KHook's own map has (`hooks_detour.find(vtable + index)`,
/// `detour.cpp:1908-1914`): the slot at [`HOOKED_SLOT`] answers [`SAVED`] - the pre-hook entry -
/// whatever the slot holds now; every other slot answers its current word. `HOOKED_SLOT == 0`
/// means "no hooked slot" and leaves [`VIRTUAL`] in charge.
static HOOKED_SLOT: AtomicUsize = AtomicUsize::new(0);
static SAVED: AtomicUsize = AtomicUsize::new(0);

/// The mock: only the two `FindOriginal` queries are modelled (see the module docs); every other
/// call is one this gate never makes and panics if that changes.
struct Mock;

impl KHookApi for Mock {
    unsafe fn setup_hook(
        &self,
        _function: *mut c_void,
        _context: *mut c_void,
        _removed_function: *mut c_void,
        _pre: *mut c_void,
        _post: *mut c_void,
        _make_return: *mut c_void,
        _make_call_original: *mut c_void,
        _stack_size: u32,
        _async: bool,
    ) -> HookId {
        unreachable!("the identity gate only asks the two FindOriginal questions")
    }

    unsafe fn setup_virtual_hook(
        &self,
        _vtable: *mut *mut c_void,
        _index: i32,
        _context: *mut c_void,
        _removed_function: *mut c_void,
        _pre: *mut c_void,
        _post: *mut c_void,
        _make_return: *mut c_void,
        _make_call_original: *mut c_void,
        _stack_size: u32,
        _async: bool,
    ) -> HookId {
        unreachable!("the identity gate only asks the two FindOriginal questions")
    }

    unsafe fn remove_hook(
        &self,
        _id: HookId,
        _async: bool,
        _hook_removal_fn: *mut c_void,
        _context: *mut c_void,
    ) {
        unreachable!("the identity gate only asks the two FindOriginal questions")
    }

    fn get_context_ptr(&self) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn get_original_function(&self) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn get_original_value_ptr(&self) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn get_override_value_ptr(&self) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn get_current_value_ptr(&self, _pop: bool) -> *mut c_void {
        core::ptr::null_mut()
    }

    fn destroy_return_value(&self) {}

    /// `detour.cpp:1897-1905`: the pre-hook function, or `function` when it is not hooked.
    fn find_original(&self, function: *mut c_void) -> *mut c_void {
        let answer = INLINE.load(Ordering::SeqCst);
        if answer == 0 {
            function
        } else {
            answer as *mut c_void
        }
    }

    /// `detour.cpp:1907-1915`: keyed on the slot ADDRESS. A slot KHook hooked answers its saved
    /// pre-hook entry; anything else answers the slot's current word (or [`VIRTUAL`] when a case
    /// wants a fixed answer, e.g. for a preferred-base table that is not mapped here).
    fn find_original_virtual(&self, vtable: *mut *mut c_void, index: i32) -> *mut c_void {
        // SAFETY: the caller of `find_original_virtual` guarantees the slot is readable
        // (`s2sdk::hook::find_original_virtual`'s contract); `offset` is the same `vtable +
        // index` pointer arithmetic KHook does (`detour.cpp:1909`).
        let slot = unsafe { vtable.offset(index as isize) } as usize;
        let hooked = HOOKED_SLOT.load(Ordering::SeqCst);
        if hooked != 0 && slot == hooked {
            return SAVED.load(Ordering::SeqCst) as *mut c_void;
        }
        let answer = VIRTUAL.load(Ordering::SeqCst);
        if answer != 0 {
            return answer as *mut c_void;
        }
        // SAFETY: as above - KHook's own answer for an unhooked slot reads it.
        unsafe { *vtable.offset(index as isize) }
    }

    unsafe fn do_recall(
        &self,
        _action: Action,
        _ptr_to_return: *mut c_void,
        _return_size: usize,
        _init_op: *mut c_void,
        _deinit_op: *mut c_void,
    ) -> *mut c_void {
        unreachable!("the identity gate only asks the two FindOriginal questions")
    }

    unsafe fn save_return_value(
        &self,
        _action: Action,
        _ptr_to_return: *mut c_void,
        _return_size: usize,
        _init_op: *mut c_void,
        _deinit_op: *mut c_void,
        _original: bool,
    ) {
        unreachable!("the identity gate only asks the two FindOriginal questions")
    }

    fn lookup_signature(
        &self,
        _start: *mut c_void,
        _size: usize,
        _signature: *const core::ffi::c_char,
    ) -> *mut c_void {
        unreachable!("the identity gate only asks the two FindOriginal questions")
    }

    fn was_original_function_skipped(&self) -> bool {
        false
    }
}

/// The two answers this binary's mock gives. Serialised by [`TEST_LOCK`]: the API slot is
/// process-wide, like `s2sdk::hook` says.
static INSTALLED: AtomicBool = AtomicBool::new(false);
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Install the mock with `(inline, virtual)` answers for the duration of `body`, then remove it.
///
/// The answers are "addresses the mock answers"; `0` means "answer the input unchanged", which
/// is KHook's answer for a target it never hooked (`detour.cpp:1897-1915`).
fn with_mock<R>(inline: usize, virt: usize, body: impl FnOnce() -> R) -> R {
    with_slot_mock(inline, virt, 0, 0, body)
}

/// [`with_mock`] with the address-keyed answer set too: the slot at `hooked` (an address)
/// answers `saved`, as KHook's map does (`detour.cpp:1908-1914`).
fn with_slot_mock<R>(
    inline: usize,
    virt: usize,
    hooked: usize,
    saved: usize,
    body: impl FnOnce() -> R,
) -> R {
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !INSTALLED.load(Ordering::SeqCst),
        "a previous case left the mock installed"
    );
    INLINE.store(inline, Ordering::SeqCst);
    VIRTUAL.store(virt, Ordering::SeqCst);
    HOOKED_SLOT.store(hooked, Ordering::SeqCst);
    SAVED.store(saved, Ordering::SeqCst);
    let mock: &'static Mock = Box::leak(Box::new(Mock));
    s2sdk::hook::install(mock);
    INSTALLED.store(true, Ordering::SeqCst);
    let out = body();
    s2sdk::hook::uninstall();
    INSTALLED.store(false, Ordering::SeqCst);
    HOOKED_SLOT.store(0, Ordering::SeqCst);
    SAVED.store(0, Ordering::SeqCst);
    out
}

fn no_mock<R>(body: impl FnOnce() -> R) -> R {
    let _lock = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !INSTALLED.load(Ordering::SeqCst),
        "a previous case left the mock installed"
    );
    HOOKED_SLOT.store(0, Ordering::SeqCst);
    SAVED.store(0, Ordering::SeqCst);
    body()
}

/// The `what` of each relaxation, in the order the check met them.
fn relaxed_names(relaxed: &[Relaxation]) -> Vec<&'static str> {
    relaxed.iter().map(|r| r.what).collect()
}

// ---------------------------------------------------------------------------------------------
// `slot_matches` / `obj_is_in` (the object identity check).
// ---------------------------------------------------------------------------------------------

/// A fabricated table for the object check: `base` is the table's own address, so
/// `base + vt_rva` (with `vt_rva = 0`) is where `obj_is_in` reads the slots from.
fn table_with_slot(word: u64) -> Vec<u64> {
    let mut words = vec![0u64; 8];
    words[usize::from(VT_SLOT)] = word;
    words
}

/// The pin the object tests use: the fabricated table is the vtable (`vt_rva = 0`) and the
/// pinned target is `base + 0x28`.
fn obj_slot_pin() -> PinSlot {
    PinSlot {
        module: 0,
        vt_rva: 0,
        slot: VT_SLOT,
        target_rva: 0x28,
        what: "SYNTH_SLOT",
    }
}

/// KHook answers the pinned target for the slot: the check passes and records the relaxation.
#[test]
fn obj_is_accepts_a_khooked_slot_and_records_it() {
    scratch_log();
    let mut table = table_with_slot(FAKE_JIT);
    let base = table.as_ptr() as u64;
    let want = base + 0x28;
    let obj = [base];
    with_mock(0, want as usize, || {
        let mut relaxed = Vec::new();
        assert!(obj_is_in_relaxed(
            obj.as_ptr().cast(),
            base,
            0,
            0,
            &[obj_slot_pin()],
            &mut relaxed
        ));
        assert_eq!(relaxed_names(&relaxed), vec!["SYNTH_SLOT"], "the pin is recorded once");
        assert!(
            relaxed[0].line.starts_with("relaxed: SYNTH_SLOT vt[3]=0x"),
            "the line carries the slot and both addresses: {}",
            relaxed[0].line
        );
        // And `slot_matches` answers the same thing (the helper both callers use).
        let mut direct = Vec::new();
        // SAFETY: the table is a live `Vec<u64>` with more than `VT_SLOT` words.
        assert!(unsafe {
            slot_matches(
                table.as_mut_ptr() as *mut *mut c_void,
                i32::from(VT_SLOT),
                FAKE_JIT,
                want,
                "SYNTH_SLOT",
                &mut direct,
            )
        });
        assert_eq!(relaxed_names(&direct), vec!["SYNTH_SLOT"]);
    });
}

/// No API: nobody to ask, so the compare is exactly the C's and fails.
#[test]
fn obj_is_refuses_a_changed_slot_without_khook() {
    scratch_log();
    no_mock(|| {
        let table = table_with_slot(FAKE_JIT);
        let base = table.as_ptr() as u64;
        let obj = [base];
        let mut relaxed = Vec::new();
        assert!(!obj_is_in_relaxed(
            obj.as_ptr().cast(),
            base,
            0,
            0,
            &[obj_slot_pin()],
            &mut relaxed
        ));
        assert!(relaxed.is_empty());
    });
}

/// PINS THE DOCUMENTED LIMITATION (review round 2): KHook hooks the slot (its saved original is
/// the pin), and then a NON-KHook raw write replaces KHook's thunk. The check **passes**, because
/// `FindOriginalVirtual` is keyed on the slot ADDRESS: it finds the entry in `hooks_detour` and
/// answers the saved original whatever the slot holds now (`detour.cpp:1908-1914`), and `IKHook`
/// has no query that asks "is this word your thunk" (`khook.hpp:2195-2196` is the whole surface).
///
/// The mock keys on the slot address for exactly that reason, the way K's map does. The failure
/// mode this leaves is a JIT/corruption write on an already-KHook'd slot; the bridge cannot tell
/// it from KHook's own patch, and this test is where that is written down instead of claimed
/// closed.
#[test]
fn raw_write_over_a_khooked_slot_passes_identity() {
    scratch_log();
    const RAW: u64 = 0x7f00_0bad_0bad_0000;
    let mut table = table_with_slot(FAKE_JIT); // KHook's thunk in the slot first
    let base = table.as_ptr() as u64;
    let want = base + 0x28; // the pin, and the original KHook saved
    let slot = table.as_ptr() as usize + usize::from(VT_SLOT) * 8;
    table[usize::from(VT_SLOT)] = RAW; // then somebody's raw write over it
    let obj = [base];
    with_slot_mock(0, 0, slot, want as usize, || {
        let mut relaxed = Vec::new();
        assert!(
            obj_is_in_relaxed(
                obj.as_ptr().cast(),
                base,
                0,
                0,
                &[obj_slot_pin()],
                &mut relaxed
            ),
            "the documented limitation: an address-keyed FindOriginalVirtual still answers the \
             original for a slot a raw write replaced (relaxed={:?})",
            relaxed_names(&relaxed)
        );
        assert_eq!(relaxed_names(&relaxed), vec!["SYNTH_SLOT"], "recorded as relaxed");
    });
}

/// A raw write KHook does not know about (`FindOriginalVirtual` answers the current word) fails
/// closed: that is the check that stops a JIT hook or a corruption from passing as a pin.
#[test]
fn obj_is_refuses_a_raw_write_that_khook_does_not_know() {
    scratch_log();
    let table = table_with_slot(FAKE_JIT);
    let base = table.as_ptr() as u64;
    let obj = [base];
    with_mock(0, 0, || {
        let mut relaxed = Vec::new();
        assert!(!obj_is_in_relaxed(
            obj.as_ptr().cast(),
            base,
            0,
            0,
            &[obj_slot_pin()],
            &mut relaxed
        ));
        assert!(relaxed.is_empty(), "a raw write is not a relaxation");
    });
}

// ---------------------------------------------------------------------------------------------
// `verify_one` (the module gate).
// ---------------------------------------------------------------------------------------------

/// A KHook'd slot verifies and is reported as relaxed.
#[test]
fn verify_one_relaxes_a_khooked_vtable_slot() {
    scratch_log();
    let mut fixture = Fixture::new();
    fixture.hook_slot();
    with_mock(0, (BASE + u64::from(TARGET_RVA)) as usize, || {
        let mut relaxed = Vec::new();
        assert_eq!(fixture.verify(&mut relaxed), Ok(()));
        assert_eq!(relaxed_names(&relaxed), vec!["SYNTH_SLOT"]);
    });
}

/// Without an API the same image fails the slot pin, exactly as the C does.
#[test]
fn verify_one_fails_a_khooked_slot_without_khook() {
    scratch_log();
    let mut fixture = Fixture::new();
    fixture.hook_slot();
    no_mock(|| {
        let mut relaxed = Vec::new();
        assert_eq!(fixture.verify(&mut relaxed), Err(Fail::SlotPin("SYNTH_SLOT")));
        assert!(relaxed.is_empty());
    });
}

/// A raw write (`FindOriginalVirtual` answers the slot's current word) fails closed.
///
/// The answer is `FAKE_JIT` itself rather than "read the slot": `verify_one` hands the mock the
/// *live* table address (`base + vt_rva`), which in this synthetic image is a preferred-base
/// number and not mapped in this process - a mock that dereferenced it would fault, and the
/// real KHook reading its own `hooks_detour` map has no such problem.
#[test]
fn verify_one_fails_a_raw_slot_write() {
    scratch_log();
    let mut fixture = Fixture::new();
    fixture.hook_slot();
    with_mock(0, FAKE_JIT as usize, || {
        let mut relaxed = Vec::new();
        assert_eq!(fixture.verify(&mut relaxed), Err(Fail::SlotPin("SYNTH_SLOT")));
        assert!(relaxed.is_empty());
    });
}

/// A foreign KHook inline detour on the pinned function skips that prologue compare and is
/// recorded - the bytes there are KHook's jump, not the game's prologue.
#[test]
fn verify_one_skips_a_byte_pin_with_a_khook_inline_detour() {
    scratch_log();
    let mut fixture = Fixture::new();
    fixture.hook_prologue();
    let address = BASE + u64::from(PROLOGUE_RVA);
    with_mock((address + 0x40) as usize, 0, || {
        let mut relaxed = Vec::new();
        assert_eq!(fixture.verify(&mut relaxed), Ok(()));
        assert_eq!(relaxed_names(&relaxed), vec!["SYNTH_PROLOGUE"]);
    });
}

/// `FindOriginal` answering the function itself means KHook has no detour there, so the
/// prologue compare runs - and fails on the bytes KHook would have written.
#[test]
fn verify_one_does_not_skip_a_byte_pin_without_a_detour() {
    scratch_log();
    let mut fixture = Fixture::new();
    fixture.hook_prologue();
    let address = BASE + u64::from(PROLOGUE_RVA);
    with_mock(address as usize, 0, || {
        let mut relaxed = Vec::new();
        assert_eq!(
            fixture.verify(&mut relaxed),
            Err(Fail::BytePin("SYNTH_PROLOGUE"))
        );
        assert!(relaxed.is_empty(), "no relaxation was made");
    });
}

/// The unmodified fixture still verifies with the mock installed (nothing to relax), so the
/// mock's presence alone never changes a passing gate.
#[test]
fn verify_one_accepts_the_pristine_image_with_a_mock_installed() {
    scratch_log();
    let fixture = Fixture::new();
    with_mock(0, 0, || {
        let mut relaxed = Vec::new();
        assert_eq!(fixture.verify(&mut relaxed), Ok(()));
        assert!(relaxed.is_empty());
    });
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
        let path = dir.join("cs2-bridge-khook-identity-test.log");
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        path
    })
    .clone()
}
