//! Class identity from an object's vtable, through the module's own RTTI.
//!
//! The C bridge proves an object's class by comparing its vptr with a pinned vtable RVA
//! (`obj_is`, `bridge/mcp_bridge.c:1622-1641`). Those RVAs move with every game update; the
//! RTTI record the compiler wrote next to the vtable does not. [`class_name`] reads that record
//! and returns the class, [`is_class`] is the boolean form of the same question.
//!
//! # The image model
//!
//! Every function here is pure over an [`ImageRange`]: the module's mapped image as an
//! RVA-indexed byte slice plus the address it is mapped at, i.e. the same shape
//! RVA-indexed byte image plus the address it is mapped at, i.e. the same shape
//! `cs2-bridge`'s `pe::map_image` produces for a file and `Module::image` describes for a live
//! module. Nothing dereferences a pointer to decide whether it is safe: a vptr, `vptr[-1]`,
//! a locator, a type descriptor or a name outside `[base, base + len)` is
//! [`RttiError::OutsideAddress`] (or [`RttiError::OutsideRva`] for a field stored as an RVA),
//! decided **before** the read. A read that would fault therefore cannot happen through this
//! module.
//!
//! # Both ABIs, selected by the image
//!
//! Both readers are compiled on every target; [`ImageRange::abi`] (or
//! [`ImageRange::of_image`], which sniffs the `MZ`/`ELF` magic) picks one. That is the object's
//! module kind, not a `cfg`: a Windows module is a PE image and carries Microsoft RTTI, a Linux
//! module is an ELF image and carries Itanium RTTI.
//!
//! ## MSVC x64
//!
//! `vptr[-1]` is an absolute `RTTICompleteObjectLocator*` (`vcruntime.h`,
//! `_RTTICompleteObjectLocator`; the layout Microsoft's own `__RTDynamicCast` walks). The
//! locator is 24 bytes: `signature` (u32, 1 on x64 - the x86 layout, signature 0, stores
//! absolute pointers and is refused), `offset` (u32), `cdOffset` (u32), `pTypeDescriptor`
//! (i32 RVA), `pClassDescriptor` (i32 RVA), `pSelf` (i32 RVA, which must equal the locator's own
//! RVA: a locator that disagrees with where it was found is stale or forged and is refused).
//! The type descriptor is a `TypeDescriptor` (`vcruntime.h`, `_TypeDescriptor`): `pVFTable`,
//! `spare`, then the name; the name starts at +16 and is `.?AV<name>@@` for a class,
//! `.?AU<name>@@` for a struct (also `.?AT` union, `.?AW` enum).
//!
//! Measured against the staged 2000930 Windows bins: every one of the 51 pinned vtables
//! resolves, e.g. `hammer.dll` RVA `0x175ad00` -> locator `0x1e6f208` (signature 1, pSelf
//! equal), type descriptor `0x228a370`, name `.?AVCMapEntity@@`.
//!
//! Nested names (`.?AVFoo@Bar@@`, `Bar::Foo`) are **best effort**: the components after the
//! class name are enclosing scopes, so they are reversed and joined with `::`; a name that is
//! not a plain identifier path (a template instantiation, a back-reference name) keeps its
//! decorated form as both the simple and the decorated name rather than guessing. The decorated
//! name is always kept verbatim in [`ClassName::decorated`]. Which to compare with:
//! [`is_class`]/[`is_class_at`] (`ClassName::is`) for ordinary class names,
//! [`is_class_decorated`] (`ClassName::is_decorated`) when the caller holds the exact decorated
//! string - a template, a back-reference form, or a name from the signature database.
//!
//! ## Itanium
//!
//! `vptr[-1]` is the `std::type_info*` (Itanium C++ ABI, "3.2.2 Virtual Table Layout": every
//! vtable is preceded by the offset-to-top and the type_info pointer, and the object's vptr
//! points at the first function). `std::type_info`'s second field is the name
//! (`__cxxabiv1::__class_type_info`, layout `[vptr, const char *name]`), the mangled type name
//! without the `_ZTS` prefix (`typeid(T).name()`): `10CMapEntity`, or `N2ns1BE` for a nested
//! type. Only the plain `<len><ident>` and `N...E` forms are demangled; anything else (builtin
//! types, substitutions like `St`) is returned raw, and the mangled name is always kept in
//! [`ClassName::decorated`]. Two GCC spellings are understood: the `*` that marks a name with
//! internal linkage (`*N12_GLOBAL__N_13FooE`) is stripped from the simple name, and the
//! anonymous-namespace component `_GLOBAL__N_1` becomes `(anonymous namespace)`.
//!
//! # Secondary vtables and [`ClassName::top_offset`]
//!
//! A base subobject has its own vtable pointer, and both ABIs record *where in the complete
//! object* that subobject sits:
//!
//! * MSVC: every vftable has its own `RTTICompleteObjectLocator` (the same type descriptor,
//!   a different locator), and the locator's `offset` field is the vfptr's position in the
//!   complete object (`cdOffset` is the virtual-base offset). Measured on `soundsystem.dll`:
//!   `SNV_SOUNDOPSYSTEM_VT` -> locator `0x4dcc50`, `offset` `0x0`;
//!   `SNV_SOUNDEVENTMGR_VT` -> locator `0x4dcc28`, `offset` `0x8`; both type descriptor
//!   `0x553280`, `.?AVCSoundOpSystem@@`.
//! * Itanium: the word *before* the type_info pointer, `vptr[-2]`, is the offset-to-top; it is
//!   `-8` for a nominal secondary base subobject (`D`'s `C2` table measured in
//!   `tests/rtti_itanium_live.rs`) and can be positive for a virtual base.
//!
//! [`ClassName::top_offset`] reports that position in the offset-to-top convention on both
//! ABIs: `0` for the primary vtable, `-8` for a subobject one word into the complete object
//! (MSVC's `offset` is reported negated, so the sign means the same thing on both).
//! [`is_class`] therefore requires `top_offset == 0` and the dynamic type: it says "this vptr
//! is the complete object's primary vtable for `class`", not "a `class` subobject is somewhere
//! in this object". [`is_class_at`] is the same question for a known subobject offset. On both
//! ABIs the *name* of a secondary table is still the most-derived class (`CSoundOpSystem` for
//! both of its audio-system tables; `D` for both tables of a multiple-inheritance `D`), so the
//! offset, not the name, is what tells the tables apart.
//!
//! # Cross-module `type_info`
//!
//! An Itanium RTTI record can name a `type_info` the compiler imported from another module (an
//! undefined `_ZTI`; `std::string`'s instantiations in `libclient.so` are the measured case,
//! 18 of them). This reader does not follow it: the `type_info` pointer is checked against
//! `[base, base + size)` first, and a pointer outside it is [`RttiError::OutsideAddress`] with
//! no dereference - there is no implicit search of other modules. A caller that has the other
//! module's image can ask again through it. Microsoft RTTI has no such forward reference: a
//! locator, its type descriptor and the name all live in the image that owns the vtable, and a
//! field that points outside is [`RttiError::OutsideRva`].
//!
//! # Live images
//!
//! [`module::Module::image`](crate::module::Module::image) is the live adapter: `Module` works
//! out the mapping's base, size and readable RVA ranges once, when it finds the module, and
//! hands them back as an [`ImageRange`] whose source is a [`LiveReader`]. Every read through
//! that image is bounds-checked against the ranges *before* the pointer is dereferenced, and
//! the only bytes touched are the words the reader asks for (`copy_nonoverlapping` into a
//! local buffer); no slice is ever formed over the live mapping and no copy of the image
//! exists. The offline constructors ([`ImageRange::msvc`]/[`ImageRange::itanium`]) are the same
//! `ImageRange` over a `&[u8]`; the readers do not know which one they are reading.

use core::fmt;

/// The C++ ABI an image was built with.
///
/// Selected per image, never per target: the two are the same crate's two halves (a Windows
/// module is PE/MSVC, a Linux module is ELF/Itanium), and both readers are compiled everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    /// Microsoft's ABI, as used by `x86_64-pc-windows-msvc` (PE images).
    Msvc,
    /// The Itanium C++ ABI, as used by GCC/Clang on Linux (ELF images).
    Itanium,
}

/// One checked read from an image: copy exactly `buf.len()` bytes at RVA `rva` into `buf`.
///
/// The implementation must decide whether the whole range is readable **before** touching
/// memory; [`RttiError`] is the answer when it is not. Every read the RTTI readers make goes
/// through this trait, so the offline (`[u8]`/`Vec<u8>`) and in-place ([`LiveReader`]) images
/// share one set of checks and one reader.
pub trait ImageRead {
    /// Copy `buf.len()` bytes at `rva` into `buf`.
    ///
    /// # Errors
    ///
    /// [`RttiError::OutsideRva`] when the range is not inside the image (or not readable).
    fn read(&self, rva: u64, buf: &mut [u8], what: &'static str) -> Result<(), RttiError>;
}

/// An offline image: the slice *is* the mapped image, RVA-indexed.
impl ImageRead for [u8] {
    fn read(&self, rva: u64, buf: &mut [u8], what: &'static str) -> Result<(), RttiError> {
        let outside = || RttiError::OutsideRva {
            what,
            rva: i64::try_from(rva).unwrap_or(i64::MAX),
            len: self.len(),
        };
        let start = usize::try_from(rva).map_err(|_| outside())?;
        let end = start.checked_add(buf.len()).ok_or_else(outside)?;
        let bytes = self.get(start..end).ok_or_else(outside)?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

/// An offline image whose bytes are owned (`Vec<u8>`): the same reader as `[u8]`.
impl ImageRead for Vec<u8> {
    fn read(&self, rva: u64, buf: &mut [u8], what: &'static str) -> Result<(), RttiError> {
        self.as_slice().read(rva, buf, what)
    }
}

/// The mapped module, read in place: its load address plus the RVA ranges that are mapped
/// readable.
///
/// Nothing is copied: a read is a `copy_nonoverlapping` of exactly the requested bytes from
/// `base + rva`, and only after `[rva, rva + len)` is found inside one of the `(rva, len)`
/// ranges. The ranges come from the loader's own description of the mapping (PE section
/// characteristics on Windows, `PT_LOAD`/`PF_R` segments on Linux), computed once by
/// `Module::find_loaded`.
#[derive(Clone, Debug)]
pub struct LiveReader {
    base: u64,
    readable: Vec<(u64, u64)>,
}

impl LiveReader {
    /// A live reader over `base` with `(rva, len)` readable ranges. The caller (the module
    /// loader adapter) owns the claim that those ranges are mapped and readable.
    #[must_use]
    pub fn new(base: u64, readable: Vec<(u64, u64)>) -> Self {
        Self { base, readable }
    }

    /// The address the module is mapped at.
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// The `(rva, len)` ranges the adapter says are readable.
    #[must_use]
    pub fn readable(&self) -> &[(u64, u64)] {
        &self.readable
    }

    /// How many bytes from `rva` are inside one readable range (0 when `rva` is not readable).
    fn readable_run(&self, rva: u64) -> u64 {
        self.readable
            .iter()
            .filter(|&&(start, len)| rva >= start && rva < start.saturating_add(len))
            .map(|&(start, len)| start.saturating_add(len).saturating_sub(rva))
            .max()
            .unwrap_or(0)
    }
}

impl ImageRead for LiveReader {
    fn read(&self, rva: u64, buf: &mut [u8], what: &'static str) -> Result<(), RttiError> {
        let outside = || RttiError::OutsideRva {
            what,
            rva: i64::try_from(rva).unwrap_or(i64::MAX),
            len: self.readable.len(),
        };
        let len = u64::try_from(buf.len()).expect("a usize fits u64 on the supported targets");
        let end = rva.checked_add(len).ok_or_else(outside)?;
        // Check the whole range against the loader's readable ranges BEFORE dereferencing.
        let inside = self
            .readable
            .iter()
            .any(|&(start, range_len)| rva >= start && end <= start.saturating_add(range_len));
        if !inside {
            return Err(outside());
        }
        let address = self.base.checked_add(rva).ok_or_else(outside)?;
        // SAFETY: the loader mapped `[base + start, base + start + range_len)` readably for each
        // range (the module is loaded: `Module::find_loaded` found it, and it is not unloaded
        // while the `Module` is alive on Windows; on Linux `Module`'s `dlopen(RTLD_NOLOAD)`
        // handle pins the mapping). `[rva, rva + len)` was just checked to be inside one such
        // range, so the source pointer is valid for `buf.len()` bytes and the two buffers do
        // not overlap (one is a local `buf`).
        unsafe {
            core::ptr::copy_nonoverlapping(address as *const u8, buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }
}

/// Where an [`ImageRange`]'s bytes come from: an offline slice or the live mapping.
#[derive(Clone, Debug)]
enum Source<'a> {
    Bytes(&'a [u8]),
    Live(LiveReader),
}

impl Source<'_> {
    fn read(&self, rva: u64, buf: &mut [u8], what: &'static str) -> Result<(), RttiError> {
        match self {
            Self::Bytes(bytes) => bytes.read(rva, buf, what),
            Self::Live(live) => live.read(rva, buf, what),
        }
    }

    /// How many bytes from `rva` are readable (the offline image is one range).
    fn readable_run(&self, rva: u64, size: u64) -> u64 {
        match self {
            Self::Bytes(_) => size.saturating_sub(rva),
            Self::Live(live) => live.readable_run(rva),
        }
    }
}

/// A module image in the shape the readers need: `size` bytes indexed by RVA, mapped at
/// `base`, plus the ABI.
///
/// Two sources, one reader: the offline constructors ([`ImageRange::msvc`],
/// [`ImageRange::itanium`]) borrow an RVA-indexed `&[u8]`, and `Module::image()` builds the
/// in-place [`LiveReader`]. Every field read is bounded against `size` and, for the live
/// source, against the loader's readable ranges, before the read.
///
/// [`ImageRange::of_image`] sniffs the magic and sets [`Abi`]; the explicit constructors exist
/// for images whose format is known (a mapped PE file, a relocated-at-zero ELF file).
#[derive(Clone, Debug)]
pub struct ImageRange<'a> {
    base: u64,
    size: u64,
    abi: Abi,
    source: Source<'a>,
}

impl<'a> ImageRange<'a> {
    /// An offline image of the given ABI, mapped at `base`.
    #[must_use]
    pub const fn new(bytes: &'a [u8], base: u64, abi: Abi) -> Self {
        Self {
            base,
            size: bytes.len() as u64,
            abi,
            source: Source::Bytes(bytes),
        }
    }

    /// A PE image (MSVC RTTI) mapped at `base`.
    #[must_use]
    pub const fn msvc(bytes: &'a [u8], base: u64) -> Self {
        Self::new(bytes, base, Abi::Msvc)
    }

    /// An ELF image (Itanium RTTI) mapped at `base`.
    ///
    /// The bytes must be an image *as the loader maps it for that base*: the pointer slots
    /// this module reads (`vptr[-1]`, the `type_info` name) must already hold the addresses
    /// the base implies. A file copied verbatim is only such an image at base 0 when its
    /// relocations are self-relative (the tests apply them; see
    /// `tests/rtti_real.rs`'s ELF half).
    #[must_use]
    pub const fn itanium(bytes: &'a [u8], base: u64) -> Self {
        Self::new(bytes, base, Abi::Itanium)
    }

    /// An in-place image of a mapped module: `base`, the mapped span `size` and the
    /// `(rva, len)` ranges the loader says are readable.
    #[must_use]
    pub fn live(base: u64, size: u64, readable: Vec<(u64, u64)>, abi: Abi) -> Self {
        Self {
            base,
            size,
            abi,
            source: Source::Live(LiveReader::new(base, readable)),
        }
    }

    /// An image whose ABI comes from its format: `MZ` (PE) -> [`Abi::Msvc`], `\x7fELF` ->
    /// [`Abi::Itanium`]. This is the "module kind" selection both readers share.
    ///
    /// # Errors
    ///
    /// [`RttiError::UnknownImageFormat`] when the image is neither.
    pub fn of_image(bytes: &'a [u8], base: u64) -> Result<Self, RttiError> {
        let abi = if bytes.starts_with(b"MZ") {
            Abi::Msvc
        } else if bytes.starts_with(b"\x7fELF") {
            Abi::Itanium
        } else {
            return Err(RttiError::UnknownImageFormat);
        };
        Ok(Self::new(bytes, base, abi))
    }

    /// The image's mapped size in bytes (RVA 0 through `size - 1`).
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The address the image is mapped at (the value RVA 0 means).
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// One past the last mapped byte's address.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.base.saturating_add(self.size)
    }

    /// Which ABI's reader [`class_name`] uses.
    #[must_use]
    pub const fn abi(&self) -> Abi {
        self.abi
    }

    /// The `(rva, len)` ranges the live adapter says are readable; empty for an offline
    /// `&[u8]` image (whose whole slice is readable by construction).
    #[must_use]
    pub fn readable(&self) -> &[(u64, u64)] {
        match &self.source {
            Source::Bytes(_) => &[],
            Source::Live(live) => live.readable(),
        }
    }

    /// Copy `buf.len()` bytes at `rva` through the same checks the RTTI reader uses.
    ///
    /// # Errors
    ///
    /// [`RttiError::OutsideRva`] when the range is outside the image or not mapped readable.
    pub fn read(&self, rva: u64, buf: &mut [u8]) -> Result<(), RttiError> {
        if rva
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err(RttiError::OutsideRva {
                what: "the module image",
                rva: i64::try_from(rva).unwrap_or(i64::MAX),
                len: usize::try_from(self.size).unwrap_or(usize::MAX),
            });
        }
        self.source.read(rva, buf, "the module image")
    }

    /// The RVA of `address`, if the whole of it is inside the image. A null address is never
    /// inside: no RTTI field is allowed to be null-and-therefore-offset-zero.
    fn rva_of(&self, address: u64, what: &'static str) -> Result<usize, RttiError> {
        let out = || RttiError::OutsideAddress {
            what,
            address,
            base: self.base,
            end: self.end(),
        };
        if address == 0 || address < self.base {
            return Err(out());
        }
        let rva = usize::try_from(address - self.base).map_err(|_| out())?;
        if u64::try_from(rva).unwrap_or(u64::MAX) >= self.size {
            return Err(out());
        }
        Ok(rva)
    }

    /// The RVA of a field that is *stored* as an image-relative offset (the MSVC locator's
    /// `pTypeDescriptor`/`pSelf`). Those are signed 32-bit in the x64 layout, so a negative
    /// value is not an RVA at all.
    fn relative(&self, rva: i32, what: &'static str) -> Result<usize, RttiError> {
        let out = || RttiError::OutsideRva {
            what,
            rva: i64::from(rva),
            len: usize::try_from(self.size).unwrap_or(usize::MAX),
        };
        let checked = usize::try_from(rva).map_err(|_| out())?;
        if checked == 0 || u64::try_from(checked).unwrap_or(u64::MAX) >= self.size {
            return Err(out());
        }
        Ok(checked)
    }

    /// `buf.len()` bytes at `rva`, or [`RttiError::OutsideRva`] if that crosses the image (or a
    /// readable range). The span check with the caller's `what` runs first.
    fn read_at(&self, rva: usize, buf: &mut [u8], what: &'static str) -> Result<(), RttiError> {
        let outside = || RttiError::OutsideRva {
            what,
            rva: i64::try_from(rva).unwrap_or(i64::MAX),
            len: usize::try_from(self.size).unwrap_or(usize::MAX),
        };
        let end = rva.checked_add(buf.len()).ok_or_else(outside)?;
        if u64::try_from(end).unwrap_or(u64::MAX) > self.size {
            return Err(outside());
        }
        self.source.read(rva as u64, buf, what)
    }

    fn u64_at(&self, rva: usize, what: &'static str) -> Result<u64, RttiError> {
        let mut bytes = [0u8; 8];
        self.read_at(rva, &mut bytes, what)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn i64_at(&self, rva: usize, what: &'static str) -> Result<i64, RttiError> {
        let mut bytes = [0u8; 8];
        self.read_at(rva, &mut bytes, what)?;
        Ok(i64::from_le_bytes(bytes))
    }

    /// The NUL-terminated string at `rva`, which must end inside the image and within the
    /// first [`NAME_SCAN_MAX`] bytes (a name field is never longer; the cap is what keeps a
    /// malformed image from making this scan the whole module).
    fn cstr_at(&self, rva: usize, what: &'static str) -> Result<String, RttiError> {
        let start = rva as u64;
        if start >= self.size {
            return Err(RttiError::OutsideRva {
                what,
                rva: i64::try_from(rva).unwrap_or(i64::MAX),
                len: usize::try_from(self.size).unwrap_or(usize::MAX),
            });
        }
        let limit = (self.size - start).min(NAME_SCAN_MAX);
        let mut name = Vec::new();
        for offset in 0..limit {
            let at = start + offset;
            if self.source.readable_run(at, self.size) == 0 {
                // A range boundary before the NUL: the name is not terminated in a readable
                // region, which is the same answer as reaching the image's end.
                return Err(RttiError::NameNotTerminated { what, rva });
            }
            let mut byte = [0u8; 1];
            self.read_at(usize::try_from(at).unwrap_or(usize::MAX), &mut byte, what)?;
            if byte[0] == 0 {
                return core::str::from_utf8(&name)
                    .map(str::to_owned)
                    .map_err(|_| RttiError::NameNotUtf8 { what, rva });
            }
            name.push(byte[0]);
        }
        Err(RttiError::NameNotTerminated { what, rva })
    }

    /// The RVA of `vptr`, with room for the two words before it: the RTTI record slot and (on
    /// Itanium) the offset-to-top.
    fn vptr_slot(&self, vptr: u64) -> Result<usize, RttiError> {
        let rva = self.rva_of(vptr, "the object's vptr")?;
        // `vptr[-1]` occupies `[rva - 8, rva)` and `vptr[-2]` `[rva - 16, rva - 8)`.
        if rva < 16 {
            return Err(RttiError::NoRttiSlot {
                vptr,
                base: self.base,
            });
        }
        Ok(rva)
    }
}

/// An MSVC `RTTICompleteObjectLocator` is 24 bytes in the x64 layout (`vcruntime.h`,
/// `_RTTICompleteObjectLocator`: 3 x u32 then 3 x i32 RVAs). The reader bounds every field it
/// takes out of it against the image, so the size is documentation, not a pre-check.
const MSVC_LOCATOR_BYTES: usize = 24;
/// `RTTICompleteObjectLocator::signature` for the x64 layout. 0 is the x86 layout (absolute
/// pointers, 28 bytes), which this reader does not pretend to understand.
const MSVC_LOCATOR_SIGNATURE_X64: u32 = 1;
/// `RTTICompleteObjectLocator::pSelf`'s offset in the x64 layout.
const MSVC_LOCATOR_P_SELF: usize = 20;
/// `RTTICompleteObjectLocator::offset`'s offset in the x64 layout: the vfptr's position in the
/// complete object (0 for the primary table, 8 for a nominal secondary base subobject).
const MSVC_LOCATOR_OFFSET: usize = 4;
/// `RTTICompleteObjectLocator::pTypeDescriptor`'s offset in the x64 layout.
const MSVC_LOCATOR_P_TYPE_DESCRIPTOR: usize = 12;
/// `TypeDescriptor::name`'s offset: `pVFTable` + `spare`, 8 bytes each (`vcruntime.h`,
/// `_TypeDescriptor`).
const MSVC_TYPE_DESCRIPTOR_NAME: usize = 16;
/// `std::type_info::__name`'s offset: the vptr occupies the first word (`cxxabi.h`,
/// `__cxxabiv1::__class_type_info`'s `[vptr, const char *]` prefix).
const ITANIUM_TYPE_INFO_NAME: usize = 8;
/// The longest name this reader will scan for: a C++ RTTI name is never anywhere near this,
/// and the cap is what stops a malformed image from turning a name read into a full-module
/// scan (the vptr and the records are single words, so only the name needs a bound).
const NAME_SCAN_MAX: u64 = 512;

/// A class identified by its RTTI record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassName {
    /// The name exactly as the image holds it: the decorated MSVC name (`".?AVCMapEntity@@"`)
    /// or the Itanium mangled name (`"10CMapEntity"`).
    decorated: String,
    /// The best-effort plain name: `"CMapEntity"`, `"Bar::Foo"` for `. ?AVFoo@Bar@@`, and the
    /// decorated name itself when it cannot be reduced without guessing.
    simple: String,
    /// The vfptr's position relative to the complete object, in the offset-to-top convention:
    /// `0` for the primary table, negative for a subobject this many bytes into the object
    /// (MSVC `-locator.offset`, Itanium `vptr[-2]`). See the module docs.
    top_offset: i64,
}

impl ClassName {
    fn new(decorated: &str, simple: String, top_offset: i64) -> Self {
        Self {
            decorated: decorated.to_owned(),
            simple,
            top_offset,
        }
    }

    /// The name as stored in the image (decorated for MSVC, mangled for Itanium).
    #[must_use]
    pub fn decorated(&self) -> &str {
        &self.decorated
    }

    /// The plain form of the name; see [`ClassName`].
    #[must_use]
    pub fn simple(&self) -> &str {
        &self.simple
    }

    /// The vfptr's position relative to the complete object (offset-to-top): `0` for the
    /// primary vtable, `-8` for the secondary table of a base subobject one word in, positive
    /// for a virtual base (Itanium only). See the module docs.
    #[must_use]
    pub const fn top_offset(&self) -> i64 {
        self.top_offset
    }

    /// Is this class exactly `class`, compared with [`ClassName::simple`]?
    ///
    /// This is the form to use for a class whose RTTI name is a plain identifier path
    /// (`CMapEntity`, `Bar::Foo`).
    #[must_use]
    pub fn is(&self, class: &str) -> bool {
        self.simple == class
    }

    /// Is this class exactly `decorated` as the image spells it (MSVC `.?AV…@@` / `.?A…@`
    /// back-reference forms, Itanium mangled names, GCC's `*` included)?
    ///
    /// This is the form to use when the caller holds the exact name rather than a class name:
    /// a template instantiation (`.?AV?$CUtlVector@H@@`, whose [`ClassName::simple`] is the
    /// same string), a back-reference name, or a name read out of the signature database. For
    /// a plain class, [`ClassName::is`] (and [`is_class`]) is the readable form; for anything
    /// the demangler reduces to exactly itself the two agree.
    #[must_use]
    pub fn is_decorated(&self, decorated: &str) -> bool {
        self.decorated == decorated
    }
}

impl fmt::Display for ClassName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.simple)
    }
}

/// Why an RTTI lookup could not answer, or refused to.
///
/// Every variant that names a location is the result of a bounds check that ran **before** the
/// read; none of them can be caused by a fault.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RttiError {
    /// The image is neither a PE (`MZ`) nor an ELF (`\x7fELF`) image, so there is no ABI to
    /// read it with.
    #[error("the image is neither a PE (`MZ`) nor an ELF (`\\x7fELF`) image")]
    UnknownImageFormat,
    /// An address that must be inside the image is not.
    #[error("{what} {address:#x} is outside the module image [{base:#x}, {end:#x})")]
    OutsideAddress {
        /// Which pointer was followed.
        what: &'static str,
        /// The address the image supplied.
        address: u64,
        /// The image's base.
        base: u64,
        /// One past the image's last byte.
        end: u64,
    },
    /// A field stored as an image-relative offset points outside the image (or is negative).
    #[error("{what} rva {rva:#x} is outside the module image (len {len:#x})")]
    OutsideRva {
        /// Which field was read.
        what: &'static str,
        /// The offset the image supplied.
        rva: i64,
        /// The image's length.
        len: usize,
    },
    /// The vptr is fewer than two words into the image, so there is no RTTI slot before it.
    #[error("vptr {vptr:#x} has no RTTI slot before it (the image starts at {base:#x})")]
    NoRttiSlot {
        /// The vptr that was asked about.
        vptr: u64,
        /// The image's base.
        base: u64,
    },
    /// An MSVC locator whose `signature` is not the x64 layout's 1.
    #[error("the MSVC RTTI locator at rva {locator:#x} has signature {signature}, not the x64 layout's 1")]
    LocatorSignature {
        /// The locator's RVA.
        locator: usize,
        /// The signature it held.
        signature: u32,
    },
    /// An MSVC locator whose `pSelf` is not its own RVA.
    #[error("the MSVC RTTI locator at rva {locator:#x} says pSelf is {p_self:#x}")]
    LocatorSelfMismatch {
        /// The locator's RVA.
        locator: usize,
        /// The `pSelf` it held.
        p_self: i32,
    },
    /// A string field with no NUL anywhere in the rest of the image.
    #[error("{what} at rva {rva:#x} is not NUL-terminated inside the image")]
    NameNotTerminated {
        /// Which string was read.
        what: &'static str,
        /// Where it starts.
        rva: usize,
    },
    /// A string field that is not UTF-8.
    #[error("{what} at rva {rva:#x} is not UTF-8")]
    NameNotUtf8 {
        /// Which string was read.
        what: &'static str,
        /// Where it starts.
        rva: usize,
    },
    /// An MSVC type descriptor whose name is not a `.?A…@@` RTTI name.
    #[error("the MSVC type descriptor name {name:?} is not a `.?A<kind><name>@@` RTTI name")]
    NotRttiName {
        /// The name the type descriptor held.
        name: String,
    },
}

/// The class of the object whose vptr is `vptr`, from the module image `image`.
///
/// `vptr` is the object's first word - the address the compiler put in it - not the object's
/// address: reading the object is the caller's business (and may be a heap address that is not
/// in any image). Everything below `vptr` is read from `image`, bounds-checked.
///
/// The answer is the *complete* object's class on both ABIs, including for a vptr that names a
/// base subobject's vtable; [`ClassName::top_offset`] says which table it was.
///
/// # Errors
///
/// Any [`RttiError`] variant: a pointer outside the image, a locator that is not the x64
/// layout or disagrees with its own address, a name that is not terminated, etc. A module built
/// without RTTI fails here rather than returning a guess.
pub fn class_name(vptr: u64, image: &ImageRange<'_>) -> Result<ClassName, RttiError> {
    match image.abi() {
        Abi::Msvc => msvc_class_name(vptr, image),
        Abi::Itanium => itanium_class_name(vptr, image),
    }
}

/// Is `vptr` the primary vtable of an object of exactly `class`? [`class_name`] compared with
/// `class`, and [`ClassName::top_offset`] required to be 0.
///
/// A secondary (base-subobject) vtable of the same class does **not** match, even though its
/// RTTI name is the complete class: a primary table always has offset 0, and a bridge proving
/// "this pointer is a `class` object" wants the primary one. [`is_class_at`] is the same
/// question for a known subobject offset.
///
/// An RTTI failure is an `Err`, never `false` - "could not tell" and "is not" are different
/// answers and the bridge needs to see the first.
///
/// # Errors
///
/// As [`class_name`].
pub fn is_class(vptr: u64, image: &ImageRange<'_>, class: &str) -> Result<bool, RttiError> {
    is_class_at(vptr, image, class, 0)
}

/// Is `vptr` the vtable of a `class` subobject at `top_offset` (offset-to-top: 0 for the
/// primary table, `-8` for a base subobject one word into the object, positive for a virtual
/// base)? The name must be exactly `class` *and* the offset exactly `top_offset`.
///
/// # Errors
///
/// As [`class_name`].
pub fn is_class_at(
    vptr: u64,
    image: &ImageRange<'_>,
    class: &str,
    top_offset: i64,
) -> Result<bool, RttiError> {
    let name = class_name(vptr, image)?;
    Ok(name.is(class) && name.top_offset == top_offset)
}

/// Is `vptr` the primary vtable of a class whose RTTI name is exactly `decorated`?
/// [`class_name`] compared with [`ClassName::is_decorated`] and offset 0.
///
/// The exact-name counterpart of [`is_class`], for names that are not a plain class name: a
/// template instantiation, a back-reference form, or the string a signature entry records.
/// Use [`is_class`]/[`is_class_at`] with the simple name for ordinary classes (for a name the
/// demangler cannot reduce, the two agree, because the simple name is the decorated one).
///
/// # Errors
///
/// As [`class_name`].
pub fn is_class_decorated(
    vptr: u64,
    image: &ImageRange<'_>,
    decorated: &str,
) -> Result<bool, RttiError> {
    let name = class_name(vptr, image)?;
    Ok(name.is_decorated(decorated) && name.top_offset == 0)
}

fn msvc_class_name(vptr: u64, image: &ImageRange<'_>) -> Result<ClassName, RttiError> {
    let slot = image.vptr_slot(vptr)?;
    let locator_pointer = image.u64_at(slot - 8, "the MSVC RTTI locator pointer")?;
    let locator = image.rva_of(locator_pointer, "the MSVC RTTI locator")?;
    // One checked read of the whole 24-byte locator; the fields below are parsed from it, so
    // every one of them is known to be inside the image.
    let mut bytes = [0u8; MSVC_LOCATOR_BYTES];
    image.read_at(locator, &mut bytes, "the MSVC RTTI locator")?;
    let u32_field =
        |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"));
    let i32_field =
        |offset: usize| i32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"));

    let signature = u32_field(0);
    if signature != MSVC_LOCATOR_SIGNATURE_X64 {
        return Err(RttiError::LocatorSignature { locator, signature });
    }
    let p_self = i32_field(MSVC_LOCATOR_P_SELF);
    if usize::try_from(p_self).ok() != Some(locator) {
        return Err(RttiError::LocatorSelfMismatch { locator, p_self });
    }
    // The locator's `offset` is the vfptr's position in the complete object; report it in the
    // offset-to-top convention (negative for a secondary table), the same sign Itanium's
    // `vptr[-2]` uses.
    let top_offset = -i64::from(u32_field(MSVC_LOCATOR_OFFSET));
    let type_descriptor = i32_field(MSVC_LOCATOR_P_TYPE_DESCRIPTOR);
    let type_descriptor = image.relative(type_descriptor, "the MSVC type descriptor")?;
    let name_rva = type_descriptor
        .checked_add(MSVC_TYPE_DESCRIPTOR_NAME)
        .ok_or(RttiError::OutsideRva {
            what: "the MSVC type descriptor name",
            rva: i64::try_from(type_descriptor).unwrap_or(i64::MAX),
            len: usize::try_from(image.size()).unwrap_or(usize::MAX),
        })?;
    let name = image.cstr_at(name_rva, "the MSVC type descriptor name")?;
    let Some(simple) = msvc_simple_name(&name) else {
        return Err(RttiError::NotRttiName { name: name.clone() });
    };
    Ok(ClassName::new(&name, simple, top_offset))
}

fn itanium_class_name(vptr: u64, image: &ImageRange<'_>) -> Result<ClassName, RttiError> {
    let slot = image.vptr_slot(vptr)?;
    // Itanium C++ ABI "3.2.2 Virtual Table Layout": the vtable group is
    // `[offset-to-top, type_info, fn0, ...]` and the object's vptr points at `fn0`.
    let top_offset = image.i64_at(slot - 16, "the Itanium offset-to-top")?;
    let type_info = image.u64_at(slot - 8, "the Itanium type_info pointer")?;
    let type_info = image.rva_of(type_info, "the Itanium type_info")?;
    let name_pointer = image.u64_at(
        type_info
            .checked_add(ITANIUM_TYPE_INFO_NAME)
            .ok_or(RttiError::OutsideRva {
                what: "the Itanium type_info name pointer",
                rva: i64::try_from(type_info).unwrap_or(i64::MAX),
                len: usize::try_from(image.size()).unwrap_or(usize::MAX),
            })?,
        "the Itanium type_info name pointer",
    )?;
    let name = image.rva_of(name_pointer, "the Itanium type_info name")?;
    let name = image.cstr_at(name, "the Itanium type_info name")?;
    let simple = itanium_simple_name(&name);
    Ok(ClassName::new(&name, simple, top_offset))
}

/// The MSVC decorations this reader understands: a type name (`.?A` + kind char) closed with
/// `@@`, whose body is an identifier or a reversed scope path.
const MSVC_TYPE_PREFIX: &str = ".?A";
const MSVC_TYPE_SUFFIX: &str = "@@";

/// `.?AVCMapEntity@@` -> `Some("CMapEntity")`, `.?AVFoo@Bar@@` -> `Some("Bar::Foo")`,
/// `.?AUPhysicsWorld_t@CPhysicsGameSystem@@` -> `Some("CPhysicsGameSystem::PhysicsWorld_t")`,
/// a body that is not a plain path (a template, an operator, a back-reference like
/// `.?AVA@B@0@`) -> `Some(<decorated>)` (kept raw rather than guessed at), and a name that is
/// not a `.?A<kind>…` type name at all -> `None` (the caller turns that into
/// [`RttiError::NotRttiName`]).
fn msvc_simple_name(decorated: &str) -> Option<String> {
    let body = decorated.strip_prefix(MSVC_TYPE_PREFIX)?;
    // `.?AV` then the body: the kind char is `V` class, `U` struct, `T` union, `W` enum.
    let (kind, body) = body.split_at_checked(1)?;
    if !matches!(kind, "V" | "U" | "T" | "W") {
        return None;
    }
    if body.is_empty() {
        return None;
    }
    // A plain type name closes with `@@`. Anything else is still an RTTI type name - Microsoft
    // emits back-reference forms (`.?AVA@B@0@`, a single trailing `@`) - but its plain form
    // needs the full demangler, so it stays decorated rather than being guessed at.
    let Some(path) = body.strip_suffix(MSVC_TYPE_SUFFIX) else {
        return Some(decorated.to_owned());
    };
    if path.is_empty() {
        return None;
    }
    let mut parts: Vec<&str> = path.split('@').collect();
    if parts.iter().any(|part| {
        part.is_empty()
            || !part.bytes().all(is_msvc_identifier_byte)
            // A digit-only fragment is a back-reference to an earlier name component, not an
            // identifier (a C++ identifier cannot start with a digit).
            || part.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        // A template instantiation (`?$CUtlVector@H`) or an operator name: a real RTTI name
        // whose plain form cannot be recovered without a demangler, so it is not guessed at.
        return Some(decorated.to_owned());
    }
    // `Foo@Bar@@` is `Bar::Foo`: the components are the class first, the enclosing scopes
    // after it.
    parts.reverse();
    Some(parts.join("::"))
}

fn is_msvc_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// `10CMapEntity` -> `CMapEntity`, `N3Foo3BarE` -> `Foo::Bar`; anything whose encoding is not
/// one of those two forms is returned unchanged.
///
/// Two GCC spellings are handled explicitly: a leading `*` (the type has internal linkage, e.g.
/// an anonymous-namespace or function-local class: `*N12_GLOBAL__N_13FooE`) is not part of the
/// name, and the anonymous namespace's component `_GLOBAL__N_1` is reported as
/// `(anonymous namespace)`.
fn itanium_simple_name(mangled: &str) -> String {
    let name = mangled.strip_prefix('*').unwrap_or(mangled);
    if let Some(simple) = itanium_single(name) {
        return simple;
    }
    if let Some(inner) = name
        .strip_prefix('N')
        .and_then(|rest| rest.strip_suffix('E'))
    {
        let mut parts = Vec::new();
        let mut rest = inner;
        while !rest.is_empty() {
            match itanium_component(rest) {
                Some((part, tail)) => {
                    parts.push(itanium_scope(part));
                    rest = tail;
                }
                // A substitution (`St`, `S_`), a template argument list: not one of the two
                // forms, so the mangled name is the answer.
                None => return mangled.to_owned(),
            }
        }
        if parts.is_empty() {
            return mangled.to_owned();
        }
        return parts.join("::");
    }
    mangled.to_owned()
}

/// The one component name this demangler renames: GCC's anonymous-namespace scope. Everything
/// else is the identifier as written.
fn itanium_scope(component: &str) -> &str {
    if component == "_GLOBAL__N_1" {
        "(anonymous namespace)"
    } else {
        component
    }
}

/// A whole-string `<len><ident>`.
fn itanium_single(mangled: &str) -> Option<String> {
    let (part, rest) = itanium_component(mangled)?;
    rest.is_empty().then(|| itanium_scope(part).to_owned())
}

/// One `<len><ident>` at the front of `mangled`: the decimal length, then exactly that many
/// identifier bytes.
fn itanium_component(mangled: &str) -> Option<(&str, &str)> {
    let digits = mangled.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let len: usize = mangled.get(..digits)?.parse().ok()?;
    let ident = mangled.get(digits..digits.checked_add(len)?)?;
    if ident.is_empty() || !ident.bytes().all(is_msvc_identifier_byte) {
        return None;
    }
    Some((ident, mangled.get(digits + len..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msvc_names_are_undecorated() {
        assert_eq!(
            msvc_simple_name(".?AVCMapEntity@@").as_deref(),
            Some("CMapEntity")
        );
        assert_eq!(
            msvc_simple_name(".?AUPhysicsWorld_t@@").as_deref(),
            Some("PhysicsWorld_t")
        );
        assert_eq!(
            msvc_simple_name(".?AVFoo@Bar@@").as_deref(),
            Some("Bar::Foo")
        );
        assert_eq!(
            msvc_simple_name(".?AUPhysicsWorld_t@CPhysicsGameSystem@@").as_deref(),
            Some("CPhysicsGameSystem::PhysicsWorld_t")
        );
    }

    #[test]
    fn msvc_names_that_are_not_a_plain_path_stay_decorated() {
        // A template instantiation: a real RTTI name whose plain form is not guessed at.
        assert_eq!(
            msvc_simple_name(".?AV?$CUtlVector@H@@").as_deref(),
            Some(".?AV?$CUtlVector@H@@")
        );
        // Back-reference forms: still a type name, but the name needs the full demangler.
        for backref in [
            ".?AVA@B@0@",      // one trailing `@`, a digit scope
            ".?AVB@A@0@@",     // a digit-only component with the proper `@@`
            ".?AVCMapEntity@", // a single trailing `@`
            ".?AV0@@",         // a bare digit component
        ] {
            assert_eq!(
                msvc_simple_name(backref).as_deref(),
                Some(backref),
                "{backref}"
            );
        }
        // Not a type name at all (`.?AV` is the type decoration; `.?AV` + kind missing).
        assert_eq!(msvc_simple_name(".?AXFoo@@"), None);
        assert_eq!(msvc_simple_name("CMapEntity"), None);
        assert_eq!(msvc_simple_name(".?AV@@"), None);
        assert_eq!(msvc_simple_name(".?AV"), None);
    }

    #[test]
    fn itanium_names_are_demangled_by_the_two_simple_forms() {
        assert_eq!(itanium_simple_name("10CMapEntity"), "CMapEntity");
        assert_eq!(itanium_simple_name("1A"), "A");
        assert_eq!(itanium_simple_name("N2ns1BE"), "ns::B");
        assert_eq!(itanium_simple_name("N3Foo3Bar1XE"), "Foo::Bar::X");
        // Builtin types and substitutions are not demangled.
        assert_eq!(itanium_simple_name("i"), "i");
        assert_eq!(itanium_simple_name("PKc"), "PKc");
        assert_eq!(itanium_simple_name("St9bad_alloc"), "St9bad_alloc");
        assert_eq!(itanium_simple_name("NSt3fooE"), "NSt3fooE");
        assert_eq!(itanium_simple_name(""), "");
    }

    #[test]
    fn itanium_internal_linkage_and_anonymous_namespaces_are_named() {
        // GCC's `*` marks internal linkage (anonymous-namespace classes, function-local ones):
        // it is not part of the type name.
        assert_eq!(
            itanium_simple_name("*N12_GLOBAL__N_13FooE"),
            "(anonymous namespace)::Foo"
        );
        assert_eq!(
            itanium_simple_name("N12_GLOBAL__N_13FooE"),
            "(anonymous namespace)::Foo"
        );
        assert_eq!(
            itanium_simple_name("N12_GLOBAL__N_13Foo4BaseE"),
            "(anonymous namespace)::Foo::Base"
        );
        // A `*` on a name that still cannot be demangled does not change the raw answer.
        assert_eq!(itanium_simple_name("*St9bad_alloc"), "*St9bad_alloc");
    }
}
