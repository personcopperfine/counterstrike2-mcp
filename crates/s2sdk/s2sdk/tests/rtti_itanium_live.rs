//! Gate: the Itanium reader over **real** Itanium RTTI - a C++ shared object this test compiles
//! with the host compiler, dlopen'd, with objects of three classes (one nested in a namespace,
//! one using multiple inheritance) handed out through `extern "C"` factories.
//!
//! Nothing about the names is written into the image by hand: the compiler writes the
//! `type_info` and the vtable, `Module::image` describes the mapping the loader made (in place:
//! no copy) and `class_name` is asked the same question the bridge will ask. The C++ source
//! asserts the one layout fact the Rust side uses (`sizeof(D) == 16`, so `D`'s `C2` subobject -
//! and its second vptr - is one word into `D`).
//!
//! The source is compiled into `CARGO_TARGET_TMPDIR` (never `/tmp`: it is RAM) on every run of
//! `cargo test -p s2sdk`, so a host without a C++ compiler fails this test loudly rather than
//! skipping it.

use std::ffi::c_void;
use std::path::Path;
use std::process::Command;

use s2sdk::module::Module;
use s2sdk::rtti::{class_name, is_class, is_class_at};

/// `A` (plain), `ns::B` (nested, derived from `A`), `D : C1, C2` (multiple inheritance) and an
/// anonymous-namespace class (GCC spells it `_GLOBAL__N_1`, sometimes with a leading `*`).
/// The factories return heap objects, so the vptrs are the compiler's own.
const SOURCE: &str = r#"
struct A {
    virtual ~A();
    virtual int value();
};
A::~A() {}
int A::value() { return 1; }

namespace ns {
struct B : A {
    ~B() override;
    int value() override;
};
B::~B() {}
int B::value() { return 2; }
}  // namespace ns

struct C1 {
    virtual ~C1();
    virtual int left();
};
struct C2 {
    virtual ~C2();
    virtual int right();
};
C1::~C1() {}
int C1::left() { return 3; }
C2::~C2() {}
int C2::right() { return 4; }

// Two vptrs: C1's at +0, C2's at +8 (the layout the Rust side relies on).
struct D : C1, C2 {
    ~D() override;
    int left() override;
    int right() override;
};
D::~D() {}
int D::left() { return 5; }
int D::right() { return 6; }
static_assert(sizeof(A) == 8, "A is one vptr");
static_assert(sizeof(D) == 16, "D is two vptrs (C1 at +0, C2 at +8)");

// Internal linkage: GCC mangles this scope `_GLOBAL__N_1` (the `*` prefix, where the compiler
// emits it, is the internal-linkage marker the Rust reader strips).
namespace {
struct Anon {
    virtual ~Anon();
    virtual int anon_value();
};
Anon::~Anon() {}
int Anon::anon_value() { return 7; }
}  // namespace
static_assert(sizeof(Anon) == 8, "Anon is one vptr");

extern "C" {
A *rtti_make_a();
ns::B *rtti_make_b();
D *rtti_make_d();
const void *rtti_make_anon();
}

A *rtti_make_a() { return new A(); }
ns::B *rtti_make_b() { return new ns::B(); }
D *rtti_make_d() { return new D(); }
const void *rtti_make_anon() { return new Anon(); }
"#;

/// Build the shared object in `CARGO_TARGET_TMPDIR` and return its path.
fn build_shared_object() -> std::path::PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rtti_itanium_live");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    let source = dir.join("live.cpp");
    let library = dir.join("librtti_live.so");
    std::fs::write(&source, SOURCE)
        .unwrap_or_else(|error| panic!("write {}: {error}", source.display()));
    let compiler = std::env::var("CXX").unwrap_or_else(|_| "c++".to_owned());
    let output = Command::new(&compiler)
        .args(["-shared", "-fPIC", "-O1", "-o"])
        .arg(&library)
        .arg(&source)
        .output()
        .unwrap_or_else(|error| {
            panic!("this gate compiles C++ with the host {compiler} (Itanium ABI): {error}")
        });
    assert!(
        output.status.success(),
        "{} failed: {}",
        compiler,
        String::from_utf8_lossy(&output.stderr)
    );
    library
}

/// The object's vptr: its first word, which the compiler wrote. Reading it is the caller's job
/// (the bridge does the same in `obj_is`); everything after this point is inside the image.
fn vptr_of(object: *const c_void) -> u64 {
    // SAFETY: every pointer passed here comes from one of the factories in `SOURCE` and is a
    // live object of a polymorphic class, whose first word is its vptr.
    unsafe { *object.cast::<u64>() }
}

#[test]
fn itanium_rtti_names_of_real_cpp_objects() {
    let library_path = build_shared_object();
    let file_name = library_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a file name");

    // The library must be mapped *before* the module is looked up; `libloading` is the dlopen.
    // SAFETY: `Library::new` runs this .so's static initialisers (there are none) and keeps it
    // mapped for as long as the `Library` lives, which is the whole test.
    let library = unsafe { libloading::Library::new(&library_path) }
        .unwrap_or_else(|error| panic!("dlopen {}: {error}", library_path.display()));

    let module = Module::find_loaded(file_name)
        .unwrap_or_else(|| panic!("{file_name} must be mapped after dlopen"));
    // The live adapter: `Module::image` describes the mapping the loader made, and the reader
    // reads it in place (no image copy exists to check).
    let image = module.image();
    println!(
        "{file_name}: base {:#x}, image {:#x} bytes, {} readable range(s) {:x?}",
        image.base(),
        image.size(),
        image.readable().len(),
        image.readable()
    );
    assert!(image.base() != 0, "a mapped .so has a load address");
    assert!(image.size() > 0, "a mapped .so has a mapped size");
    assert!(
        !image.readable().is_empty(),
        "the PT_LOAD segments are readable ranges"
    );
    // The ELF header is readable through the live image, i.e. the reader is reading the
    // mapping itself (there is no copy anywhere in `ImageRange`).
    let mut magic = [0u8; 4];
    image.read(0, &mut magic).expect("the ELF header is mapped");
    assert_eq!(&magic, b"\x7fELF");

    // A hole between two readable segments (an unmapped page inside the image's span) is
    // refused before any dereference: this is the check that protects a live mapping.
    let hole = image
        .readable()
        .windows(2)
        .find_map(|pair| {
            let end = pair[0].0 + pair[0].1;
            (end < pair[1].0).then_some(end)
        })
        .expect("a -fPIC shared object has gaps between its readable PT_LOAD segments");
    let mut byte = [0u8; 1];
    assert!(
        image.read(hole, &mut byte).is_err(),
        "rva {hole:#x} is not in any readable segment and must not be read"
    );

    // SAFETY: the three symbol names are the `extern "C"` factories `SOURCE` defines, with the
    // signature they were defined with (`T *f()`).
    let make_a: libloading::Symbol<'_, extern "C" fn() -> *const c_void> =
        unsafe { library.get(b"rtti_make_a\0") }
            .expect("rtti_make_a is the factory SOURCE exports");
    // SAFETY: as above.
    let make_b: libloading::Symbol<'_, extern "C" fn() -> *const c_void> =
        unsafe { library.get(b"rtti_make_b\0") }.expect("rtti_make_b");
    // SAFETY: as above.
    let make_d: libloading::Symbol<'_, extern "C" fn() -> *const c_void> =
        unsafe { library.get(b"rtti_make_d\0") }.expect("rtti_make_d");

    let a = (*make_a)();
    let a_name = class_name(vptr_of(a), image).expect("A's vptr resolves");
    println!(
        "A:  decorated {:?} simple {:?} offset {}",
        a_name.decorated(),
        a_name.simple(),
        a_name.top_offset()
    );
    assert_eq!(a_name.decorated(), "1A");
    assert_eq!(a_name.simple(), "A");
    assert_eq!(a_name.top_offset(), 0);
    assert_eq!(is_class(vptr_of(a), image, "A"), Ok(true));

    let b = (*make_b)();
    let b_name = class_name(vptr_of(b), image).expect("ns::B's vptr resolves");
    println!(
        "B:  decorated {:?} simple {:?} offset {}",
        b_name.decorated(),
        b_name.simple(),
        b_name.top_offset()
    );
    assert_eq!(b_name.decorated(), "N2ns1BE");
    assert_eq!(b_name.simple(), "ns::B");
    assert_eq!(b_name.top_offset(), 0);
    assert_eq!(is_class(vptr_of(b), image, "ns::B"), Ok(true));
    // `ns::B` derives from `A`, but the dynamic type is `ns::B`: the base does not match.
    assert_eq!(is_class(vptr_of(b), image, "A"), Ok(false));

    let d = (*make_d)();
    let d_name = class_name(vptr_of(d), image).expect("D's vptr resolves");
    println!(
        "D:  decorated {:?} simple {:?} offset {}",
        d_name.decorated(),
        d_name.simple(),
        d_name.top_offset()
    );
    assert_eq!(d_name.decorated(), "1D");
    assert_eq!(d_name.simple(), "D");
    assert_eq!(d_name.top_offset(), 0);
    assert_eq!(is_class(vptr_of(d), image, "D"), Ok(true));

    // The secondary vptr (C2's subobject, +8 into D as `static_assert(sizeof(D) == 16)` says):
    // Itanium's vtable group gives it the *complete* object's type_info too, so it names D,
    // not C2; `vptr[-2]` is what says it is the -8 subobject, not the primary table.
    // SAFETY: `d` is a live D; its second vptr is one word in.
    let secondary = vptr_of(unsafe { d.cast::<u8>().add(8).cast() });
    let secondary_name = class_name(secondary, image).expect("D's secondary vptr resolves");
    println!(
        "D as C2: decorated {:?} simple {:?} offset {}",
        secondary_name.decorated(),
        secondary_name.simple(),
        secondary_name.top_offset()
    );
    assert_eq!(secondary_name.simple(), "D");
    // The compiler's offset-to-top for D's C2 table is -8 (the `static_assert` above puts C2
    // one word into D); that is the value the reader must report.
    assert_eq!(secondary_name.top_offset(), -8);
    assert_eq!(is_class(secondary, image, "C2"), Ok(false));
    assert_eq!(is_class(secondary, image, "D"), Ok(false));
    assert_eq!(is_class_at(secondary, image, "D", -8), Ok(true));

    // And a vptr from outside any image is an error, not a read.
    assert!(class_name(0x1000, image).is_err());

    // The anonymous-namespace class: the compiler's own type_info name, with `_GLOBAL__N_1`
    // (and the `*` internal-linkage marker, where this compiler emits it) read as
    // `(anonymous namespace)::Anon`.
    // SAFETY: `rtti_make_anon` is the `extern "C"` factory `SOURCE` defines, with the signature
    // it was defined with (`const void *f()`).
    let make_anon: libloading::Symbol<'_, extern "C" fn() -> *const c_void> =
        unsafe { library.get(b"rtti_make_anon\0") }.expect("rtti_make_anon");
    let anon = (*make_anon)();
    let anon_name = class_name(vptr_of(anon), image).expect("Anon's vptr resolves");
    println!(
        "Anon: decorated {:?} simple {:?} offset {}",
        anon_name.decorated(),
        anon_name.simple(),
        anon_name.top_offset()
    );
    assert!(
        anon_name.decorated() == "N12_GLOBAL__N_14AnonE"
            || anon_name.decorated() == "*N12_GLOBAL__N_14AnonE",
        "the compiler's anonymous-namespace name, got {:?}",
        anon_name.decorated()
    );
    assert_eq!(anon_name.simple(), "(anonymous namespace)::Anon");
    assert_eq!(
        is_class(vptr_of(anon), image, "(anonymous namespace)::Anon"),
        Ok(true)
    );
}
