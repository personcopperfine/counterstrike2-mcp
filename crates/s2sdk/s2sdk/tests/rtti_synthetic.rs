//! The RTTI reader over images built in this test: the positive shapes of both ABIs, and the
//! negative matrix - every one of them must be an `Err` *before* the read, not a fault.
//!
//! The synthetic images below are the smallest images that have the records under test, laid
//! out by hand at RVAs of the test's choosing (`0x100` onward); they are not game addresses and
//! nothing here is a game measurement. The real binaries are the other gate
//! (`tests/rtti_real.rs`, `test = false`).

use s2sdk::rtti::{
    class_name, is_class, is_class_at, is_class_decorated, Abi, ImageRange, RttiError,
};

/// The image the reader sees: `bytes` indexed by RVA, `base` the address of index 0.
struct Image {
    bytes: Vec<u8>,
    base: u64,
}

impl Image {
    const BASE: u64 = 0x1800_0000;

    fn new(len: usize) -> Self {
        Self {
            bytes: vec![0; len],
            base: Self::BASE,
        }
    }

    fn put(&mut self, rva: usize, bytes: &[u8]) {
        self.bytes[rva..rva + bytes.len()].copy_from_slice(bytes);
    }

    fn put_u32(&mut self, rva: usize, value: u32) {
        self.put(rva, &value.to_le_bytes());
    }

    fn put_i32(&mut self, rva: usize, value: i32) {
        self.put(rva, &value.to_le_bytes());
    }

    fn put_u64(&mut self, rva: usize, value: u64) {
        self.put(rva, &value.to_le_bytes());
    }

    fn address(&self, rva: usize) -> u64 {
        self.base + rva as u64
    }

    fn msvc(&self) -> ImageRange<'_> {
        ImageRange::msvc(&self.bytes, self.base)
    }

    fn itanium(&self) -> ImageRange<'_> {
        ImageRange::itanium(&self.bytes, self.base)
    }
}

/// The MSVC image every positive/negative case starts from: a locator at `0x100`, a type
/// descriptor (name at `0x210`) at `0x200`, a vtable whose vptr is `0x300`.
///
/// The layout is `vcruntime.h`'s `_RTTICompleteObjectLocator` (`{signature, offset, cdOffset,
/// pTypeDescriptor, pClassDescriptor, pSelf}`, the last three RVAs on x64) and
/// `_TypeDescriptor` (`{pVFTable, spare, name[]}`).
const MSVC_COL: usize = 0x100;
const MSVC_TYPE_DESCRIPTOR: usize = 0x200;
const MSVC_NAME: usize = 0x210;
const MSVC_VPTR: usize = 0x300;
const MSVC_LEN: usize = 0x400;

fn msvc_image(name: &str) -> Image {
    let mut image = Image::new(MSVC_LEN);
    image.put_u32(MSVC_COL, 1); // signature: the x64 layout
    image.put_u32(MSVC_COL + 4, 0); // offset
    image.put_u32(MSVC_COL + 8, 0); // cdOffset
    image.put_i32(MSVC_COL + 12, MSVC_TYPE_DESCRIPTOR as i32); // pTypeDescriptor
    image.put_i32(MSVC_COL + 16, 0x380); // pClassDescriptor (not read)
    image.put_i32(MSVC_COL + 20, MSVC_COL as i32); // pSelf
    image.put_u64(MSVC_TYPE_DESCRIPTOR, image.address(0x3f0)); // pVFTable
    image.put(MSVC_NAME, name.as_bytes());
    image.put(MSVC_NAME + name.len(), &[0]);
    image.put_u64(MSVC_VPTR - 8, image.address(MSVC_COL)); // vptr[-1]
    image.put_u64(MSVC_VPTR, 0); // the first virtual function (not read)
    image
}

/// The Itanium image: a `type_info` at `0x200` whose name is at `0x210`, and a vptr at `0x300`.
const ITANIUM_TYPE_INFO: usize = 0x200;
const ITANIUM_NAME: usize = 0x210;
const ITANIUM_VPTR: usize = 0x300;
const ITANIUM_LEN: usize = 0x400;

fn itanium_image(name: &str) -> Image {
    let mut image = Image::new(ITANIUM_LEN);
    image.put_u64(ITANIUM_TYPE_INFO, image.address(0x3f0)); // the type_info's own vptr
    image.put_u64(ITANIUM_TYPE_INFO + 8, image.address(ITANIUM_NAME)); // name
    image.put(ITANIUM_NAME, name.as_bytes());
    image.put(ITANIUM_NAME + name.len(), &[0]);
    image.put_u64(ITANIUM_VPTR - 8, image.address(ITANIUM_TYPE_INFO)); // vptr[-1]
    image.put_u64(ITANIUM_VPTR, 0);
    image
}

#[test]
fn msvc_class_names() {
    let cases = [
        (".?AVCMapEntity@@", "CMapEntity"),
        (".?AVCSceneView@@", "CSceneView"),
        (".?AUPhysicsWorld_t@@", "PhysicsWorld_t"),
        (".?AVFoo@Bar@@", "Bar::Foo"),
        (
            ".?AUPhysicsWorld_t@CPhysicsGameSystem@@",
            "CPhysicsGameSystem::PhysicsWorld_t",
        ),
    ];
    for (decorated, simple) in cases {
        let image = msvc_image(decorated);
        let name = class_name(image.address(MSVC_VPTR), &image.msvc())
            .unwrap_or_else(|error| panic!("{decorated}: {error}"));
        assert_eq!(name.decorated(), decorated, "{decorated}");
        assert_eq!(name.simple(), simple, "{decorated}");
        assert_eq!(name.top_offset(), 0, "{decorated}: the primary table");
        assert!(is_class(image.address(MSVC_VPTR), &image.msvc(), simple).expect("resolves"));
    }
    // A template instantiation is not a plain path: the simple name stays the decorated one
    // rather than being guessed.
    let image = msvc_image(".?AV?$CUtlVector@H@@");
    let name = class_name(image.address(MSVC_VPTR), &image.msvc()).expect("resolves");
    assert_eq!(name.simple(), ".?AV?$CUtlVector@H@@");
}

#[test]
fn itanium_class_names() {
    let cases = [
        ("10CMapEntity", "CMapEntity"),
        ("1A", "A"),
        ("N2ns1BE", "ns::B"),
    ];
    for (mangled, simple) in cases {
        let image = itanium_image(mangled);
        let name = class_name(image.address(ITANIUM_VPTR), &image.itanium())
            .unwrap_or_else(|error| panic!("{mangled}: {error}"));
        assert_eq!(name.decorated(), mangled, "{mangled}");
        assert_eq!(name.simple(), simple, "{mangled}");
        assert_eq!(name.top_offset(), 0, "{mangled}: the primary table");
        assert!(is_class(image.address(ITANIUM_VPTR), &image.itanium(), simple).expect("resolves"));
    }
    // A name the demangler does not understand is returned raw.
    let image = itanium_image("St9bad_alloc");
    let name = class_name(image.address(ITANIUM_VPTR), &image.itanium()).expect("resolves");
    assert_eq!(name.decorated(), "St9bad_alloc");
    assert_eq!(name.simple(), "St9bad_alloc");
}

#[test]
fn itanium_anonymous_namespace_names() {
    // GCC's anonymous namespace is `_GLOBAL__N_1` in the mangled name, and a leading `*` marks
    // internal linkage; the decorated name keeps both, the simple name is the C++ form.
    for mangled in ["N12_GLOBAL__N_13FooE", "*N12_GLOBAL__N_13FooE"] {
        let image = itanium_image(mangled);
        let name = class_name(image.address(ITANIUM_VPTR), &image.itanium())
            .unwrap_or_else(|error| panic!("{mangled}: {error}"));
        assert_eq!(name.decorated(), mangled);
        assert_eq!(name.simple(), "(anonymous namespace)::Foo");
    }
}

#[test]
fn a_wrong_class_is_false_not_an_error() {
    let image = msvc_image(".?AVCMapEntity@@");
    let vptr = image.address(MSVC_VPTR);
    assert_eq!(is_class(vptr, &image.msvc(), "CMapEntity"), Ok(true));
    assert_eq!(is_class(vptr, &image.msvc(), "CMapWorld"), Ok(false));
    // Not even the decorated form matches: `is_class` compares the simple name only.
    assert_eq!(is_class(vptr, &image.msvc(), ".?AVCMapEntity@@"), Ok(false));
}

// ---------------------------------------------------------------------------
// Secondary vtables: the same class name, a non-zero offset-to-top
// ---------------------------------------------------------------------------

#[test]
fn secondary_vtables_have_the_subobject_offset() {
    // MSVC: the locator's `offset` is the vfptr's position in the complete object. A secondary
    // table (offset 8) is the same class, reported as -8 in the offset-to-top convention.
    let mut image = msvc_image(".?AVCSoundOpSystem@@");
    image.put_u32(MSVC_COL + 4, 8);
    let vptr = image.address(MSVC_VPTR);
    let name = class_name(vptr, &image.msvc()).expect("resolves");
    assert_eq!(name.simple(), "CSoundOpSystem");
    assert_eq!(name.top_offset(), -8);
    // `is_class` is the primary-table question, `is_class_at` the subobject one.
    assert_eq!(is_class(vptr, &image.msvc(), "CSoundOpSystem"), Ok(false));
    assert_eq!(
        is_class_at(vptr, &image.msvc(), "CSoundOpSystem", -8),
        Ok(true)
    );
    assert_eq!(
        is_class_at(vptr, &image.msvc(), "CSoundOpSystem", 0),
        Ok(false)
    );
    // The primary table is still the primary question.
    let mut primary = msvc_image(".?AVCSoundOpSystem@@");
    let vptr = primary.address(MSVC_VPTR);
    assert_eq!(
        is_class_at(vptr, &primary.msvc(), "CSoundOpSystem", 0),
        Ok(true)
    );
    // ... and an offset the table does not have is never a match.
    primary.put_u32(MSVC_COL + 4, 0x20);
    assert_eq!(is_class(vptr, &primary.msvc(), "CSoundOpSystem"), Ok(false));
}

#[test]
fn itanium_secondary_vtables_read_the_offset_to_top() {
    // Itanium: `vptr[-2]` is the offset-to-top; -8 is the compiler's value for a nominal
    // secondary base subobject one word into the complete object (measured for `D`'s `C2`
    // table in `tests/rtti_itanium_live.rs`).
    let mut image = itanium_image("1D");
    image.put(ITANIUM_VPTR - 16, &(-8i64).to_le_bytes());
    let vptr = image.address(ITANIUM_VPTR);
    let name = class_name(vptr, &image.itanium()).expect("resolves");
    assert_eq!(name.simple(), "D");
    assert_eq!(name.top_offset(), -8);
    assert_eq!(is_class(vptr, &image.itanium(), "D"), Ok(false));
    assert_eq!(is_class_at(vptr, &image.itanium(), "D", -8), Ok(true));

    // A virtual base can make the offset positive; it is reported as stored.
    let mut image = itanium_image("1D");
    image.put(ITANIUM_VPTR - 16, &16i64.to_le_bytes());
    assert_eq!(
        class_name(image.address(ITANIUM_VPTR), &image.itanium())
            .expect("resolves")
            .top_offset(),
        16
    );
}

// ---------------------------------------------------------------------------
// NEGATIVE: each of these is an `Err`, and the image slice ends where the test says, so the
// rejected read would have been out of the slice - i.e. no read could have happened.
// ---------------------------------------------------------------------------

#[test]
fn a_vptr_outside_the_image_is_refused_without_a_read() {
    // The slice ends right before the vptr: `0x300` is the last byte index, so the vptr the
    // caller claims is `0x400` - one past the end.
    let mut image = msvc_image(".?AVCMapEntity@@");
    image.bytes.truncate(MSVC_LEN);
    let vptr = image.address(MSVC_LEN);
    assert_eq!(vptr - image.base, image.bytes.len() as u64);
    assert_eq!(
        class_name(vptr, &image.msvc()),
        Err(RttiError::OutsideAddress {
            what: "the object's vptr",
            address: vptr,
            base: image.base,
            end: image.base + image.bytes.len() as u64,
        })
    );
    // A null vptr is outside too, not "RVA 0".
    assert!(class_name(0, &image.msvc()).is_err());
    // And the same for the Itanium reader.
    let image = itanium_image("10CMapEntity");
    assert!(class_name(image.address(ITANIUM_LEN), &image.itanium()).is_err());
}

#[test]
fn a_vptr_at_the_image_start_has_no_rtti_slot() {
    let image = msvc_image(".?AVCMapEntity@@");
    assert_eq!(
        class_name(image.base, &image.msvc()),
        Err(RttiError::NoRttiSlot {
            vptr: image.base,
            base: image.base,
        })
    );
    // One word in, the slot is image index 0: a valid read of an all-zero locator, which is
    // the x86 signature and therefore still an `Err` rather than a panic.
    let image2 = Image::new(MSVC_LEN);
    assert!(class_name(image2.address(8), &image2.msvc()).is_err());
}

#[test]
fn a_vptr_slot_pointing_outside_the_image_is_refused() {
    let mut image = msvc_image(".?AVCMapEntity@@");
    let outside = image.address(0x9000);
    image.put_u64(MSVC_VPTR - 8, outside);
    assert_eq!(
        class_name(image.address(MSVC_VPTR), &image.msvc()),
        Err(RttiError::OutsideAddress {
            what: "the MSVC RTTI locator",
            address: outside,
            base: image.base,
            end: image.base + image.bytes.len() as u64,
        })
    );

    let mut image = itanium_image("10CMapEntity");
    image.put_u64(ITANIUM_VPTR - 8, 0); // a null type_info
    assert!(matches!(
        class_name(image.address(ITANIUM_VPTR), &image.itanium()),
        Err(RttiError::OutsideAddress {
            what: "the Itanium type_info",
            ..
        })
    ));
    // The type_info is inside but its name pointer is not.
    let mut image = itanium_image("10CMapEntity");
    let outside = image.address(0x9000);
    image.put_u64(ITANIUM_TYPE_INFO + 8, outside);
    assert_eq!(
        class_name(image.address(ITANIUM_VPTR), &image.itanium()),
        Err(RttiError::OutsideAddress {
            what: "the Itanium type_info name",
            address: outside,
            base: image.base,
            end: image.base + image.bytes.len() as u64,
        })
    );
    // The type_info's second word crosses the end of the image (the vptr is before it, so the
    // vptr check passes and the crossing field is what stops the read).
    let mut image = Image::new(0x18c);
    image.put_u64(0x138, image.address(0x180)); // vptr[-1] -> type_info at 0x180
    assert!(matches!(
        class_name(image.address(0x140), &image.itanium()),
        Err(RttiError::OutsideRva { .. })
    ));
}

#[test]
fn an_msvc_locator_with_signature_zero_is_the_x86_layout_and_is_refused() {
    let mut image = msvc_image(".?AVCMapEntity@@");
    image.put_u32(MSVC_COL, 0);
    assert_eq!(
        class_name(image.address(MSVC_VPTR), &image.msvc()),
        Err(RttiError::LocatorSignature {
            locator: MSVC_COL,
            signature: 0,
        })
    );
}

#[test]
fn an_msvc_locator_that_disagrees_with_its_own_address_is_refused() {
    let mut image = msvc_image(".?AVCMapEntity@@");
    image.put_i32(MSVC_COL + 20, 0x140);
    assert_eq!(
        class_name(image.address(MSVC_VPTR), &image.msvc()),
        Err(RttiError::LocatorSelfMismatch {
            locator: MSVC_COL,
            p_self: 0x140,
        })
    );
}

#[test]
fn an_msvc_type_descriptor_outside_the_image_is_refused() {
    for bad in [0, -4, 0x9000] {
        let mut image = msvc_image(".?AVCMapEntity@@");
        image.put_i32(MSVC_COL + 12, bad);
        assert!(
            matches!(
                class_name(image.address(MSVC_VPTR), &image.msvc()),
                Err(RttiError::OutsideRva { .. })
            ),
            "pTypeDescriptor {bad:#x} must be refused"
        );
    }
    // A type descriptor whose name field starts past the end of the image.
    let mut image = Image::new(0x18a);
    image.put_u32(0x100, 1); // signature
    image.put_i32(0x100 + 12, 0x180); // pTypeDescriptor, after the vptr at 0x140
    image.put_i32(0x100 + 20, 0x100); // pSelf
    image.put_u64(0x138, image.address(0x100)); // vptr[-1]
    assert!(matches!(
        class_name(image.address(0x140), &image.msvc()),
        Err(RttiError::OutsideRva { .. })
    ));
}

#[test]
fn a_name_that_is_not_nul_terminated_inside_the_image_is_refused() {
    // The image ends immediately after the name's last byte: there is no NUL anywhere after
    // the name, so the scan must stop at the end and report it (the locator and the vptr are
    // before the name, so everything up to it is a valid read).
    let name = b".?AVCMapEntity@@";
    let (col, type_descriptor, vptr) = (0x100, 0x140, 0x40);
    let name_rva = type_descriptor + 16;
    let mut image = Image::new(name_rva + name.len());
    image.put_u32(col, 1);
    image.put_i32(col + 12, type_descriptor as i32);
    image.put_i32(col + 20, col as i32);
    image.put(name_rva, name);
    image.put_u64(vptr - 8, image.address(col));
    assert_eq!(
        class_name(image.address(vptr), &image.msvc()),
        Err(RttiError::NameNotTerminated {
            what: "the MSVC type descriptor name",
            rva: name_rva,
        })
    );

    let name = b"10CMapEntity";
    let (type_info, vptr) = (0x100, 0x40);
    let name_rva = type_info + 16;
    let mut image = Image::new(name_rva + name.len());
    image.put_u64(type_info + 8, image.address(name_rva));
    image.put(name_rva, name);
    image.put_u64(vptr - 8, image.address(type_info));
    assert_eq!(
        class_name(image.address(vptr), &image.itanium()),
        Err(RttiError::NameNotTerminated {
            what: "the Itanium type_info name",
            rva: name_rva,
        })
    );
}

#[test]
fn an_msvc_name_that_is_not_an_rtti_name_is_refused() {
    for bad in ["CMapEntity", ".?AXCMapEntity@@", ".?AV@@", ".?AV"] {
        let mut image = msvc_image(".?AVCMapEntity@@");
        image.put(MSVC_NAME, bad.as_bytes());
        image.put(MSVC_NAME + bad.len(), &[0]);
        assert_eq!(
            class_name(image.address(MSVC_VPTR), &image.msvc()),
            Err(RttiError::NotRttiName {
                name: bad.to_owned()
            }),
            "{bad}"
        );
    }
}

#[test]
fn msvc_backreference_names_are_kept_decorated() {
    // `. ?AVA@B@0@` and the single-trailing-`@` form are real RTTI type names (Microsoft's
    // name back-references) whose plain form needs the full demangler: the reader returns them
    // decorated instead of refusing them (`.?AV@@`-style garbage is still refused).
    for decorated in [".?AVA@B@0@", ".?AVCMapEntity@", ".?AVB@A@0@@"] {
        let mut image = msvc_image(".?AVCMapEntity@@");
        image.put(MSVC_NAME, decorated.as_bytes());
        image.put(MSVC_NAME + decorated.len(), &[0]);
        let name = class_name(image.address(MSVC_VPTR), &image.msvc())
            .unwrap_or_else(|error| panic!("{decorated} must resolve, got {error}"));
        assert_eq!(name.decorated(), decorated);
        assert_eq!(name.simple(), decorated);
        // Nothing was guessed, so the decorated match is the exact one.
        assert!(name.is_decorated(decorated));
        assert_eq!(
            is_class_decorated(image.address(MSVC_VPTR), &image.msvc(), decorated),
            Ok(true)
        );
    }
}

#[test]
fn the_exact_decorated_name_matches_template_and_odd_names() {
    // A template instantiation: the simple name is the decorated one, so `is_class` works too;
    // the exact form is what a caller holding the decorated string should use.
    let image = msvc_image(".?AV?$CUtlVector@H@@");
    let vptr = image.address(MSVC_VPTR);
    assert_eq!(
        is_class_decorated(vptr, &image.msvc(), ".?AV?$CUtlVector@H@@"),
        Ok(true)
    );
    assert_eq!(
        is_class_decorated(vptr, &image.msvc(), "CUtlVector"),
        Ok(false)
    );
    assert_eq!(
        is_class(vptr, &image.msvc(), ".?AV?$CUtlVector@H@@"),
        Ok(true),
        "for an unreduced name the simple form is the decorated one"
    );
    // A decorated match is exact, never a prefix or a case-folded match.
    assert_eq!(
        is_class_decorated(vptr, &image.msvc(), ".?AV?$CUtlVector@h@@"),
        Ok(false)
    );
    assert_eq!(
        is_class_decorated(vptr, &image.msvc(), ".?AV?$CUtlVector@H@"),
        Ok(false)
    );

    // Itanium: the decorated form is the mangled name, `*` included.
    let image = itanium_image("*N12_GLOBAL__N_13FooE");
    let vptr = image.address(ITANIUM_VPTR);
    assert_eq!(
        is_class_decorated(vptr, &image.itanium(), "*N12_GLOBAL__N_13FooE"),
        Ok(true)
    );
    assert_eq!(
        is_class_decorated(vptr, &image.itanium(), "N12_GLOBAL__N_13FooE"),
        Ok(false),
        "the image spells the `*`, so the match must too"
    );
    assert_eq!(
        is_class(vptr, &image.itanium(), "(anonymous namespace)::Foo"),
        Ok(true)
    );
}

#[test]
fn the_abi_comes_from_the_image_format() {
    let mut pe = msvc_image(".?AVCMapEntity@@");
    pe.put(0, b"MZ"); // the format magic, all `of_image` looks at
    assert_eq!(
        ImageRange::of_image(&pe.bytes, pe.base).unwrap().abi(),
        Abi::Msvc
    );
    let mut elf = itanium_image("10CMapEntity");
    elf.put(0, b"\x7fELF");
    assert_eq!(
        ImageRange::of_image(&elf.bytes, elf.base).unwrap().abi(),
        Abi::Itanium
    );
    assert!(matches!(
        ImageRange::of_image(b"not an image", 0),
        Err(RttiError::UnknownImageFormat)
    ));
    // `of_image` is the selection `class_name` obeys: the same bytes read through the sniffer
    // resolve exactly as the explicit constructor did.
    let sniffer = ImageRange::of_image(&pe.bytes, pe.base).expect("a PE image");
    assert_eq!(
        class_name(pe.address(MSVC_VPTR), &sniffer)
            .expect("resolves")
            .simple(),
        "CMapEntity"
    );
}

#[test]
fn an_image_mapped_at_zero_reads_its_own_offsets() {
    // The ELF file image in `tests/rtti_real.rs` is relocated at base 0. The reader must treat
    // stored pointers as addresses (`base + rva`), which at base 0 are the RVAs themselves.
    let mut image = itanium_image("10CMapEntity");
    image.base = 0;
    // Re-lay the pointers for base 0.
    image.put_u64(ITANIUM_TYPE_INFO, 0x3f0);
    image.put_u64(ITANIUM_TYPE_INFO + 8, ITANIUM_NAME as u64);
    image.put_u64(ITANIUM_VPTR - 8, ITANIUM_TYPE_INFO as u64);
    let name = class_name(ITANIUM_VPTR as u64, &image.itanium()).expect("resolves");
    assert_eq!(name.simple(), "CMapEntity");
    assert_eq!(image.itanium().end(), image.bytes.len() as u64);
}
