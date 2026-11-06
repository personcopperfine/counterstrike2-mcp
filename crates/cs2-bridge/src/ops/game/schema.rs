//! The game-state schema resolver and entity readers: `gs_*`
//! (`mcp_bridge_game.inc:12-485`), over the generated `s2sdk_sys` types.
//!
//! No netvar offset is compiled in: every entity field is resolved at runtime through the
//! game's own SchemaSystem (class + field name -> offset) exactly as the C does, so this
//! module holds no field offset - only the generated `CEntityIdentity` leading-block offsets
//! the C spells `S2_OFF_CEntityIdentity_*` also come from. The walk itself is the C's: scope
//! -> class binding -> fields with single-inheritance base walks, a per-process `(class,
//! field)` cache with cached misses, and the F2 layout cross-check that proves the compiled
//! `CEntityIdentity` against the live schema before anything dereferences the generated
//! offsets.
//!
//! The schema walk deliberately does NOT go through `s2sdk::schema`'s safe wrappers: those
//! call the slots directly, while every scope/interface here is first identity-checked with
//! [`obj_is`] against the pinned vtables (the C's fail-closed order, and the source of its
//! `-32017` answers). The typed generated vtables are used for the calls themselves - the
//! generator lays the Windows vtable struct out by `msvc_slot`, so a typed field IS the C's
//! `S2_SLOT_*` index. One shape difference is documented at [`gs_local_controller`]: the
//! generated `GetLocalPlayer` field takes the MSVC hidden-return pointer on Windows and the
//! header's by-value form elsewhere - both the generator's own, per target.
//!
//! All of the state here is the C's file statics (`g_gsScope`, `g_gsCache`, `g_gsLayout`,
//! `g_gsIdentSize`, `g_gsRulesIdx`); it is touched on the main thread only (the tick's drain,
//! like the C), which is what the [`GS`] mutex documents and guards.
//!
//! One shape change, no wire difference: the C's bounded name-copy ring (`g_gsNames`, four
//! 256-byte slots) exists so callers can hold up to four copied names by pointer. The readers
//! here hand out owned copies ([`GsName`], same 256-byte bound, same page-checked copy), so
//! no ring is needed and no pointer into game state escapes.

use core::ffi::c_void;
use core::mem::offset_of;
use core::ptr::addr_of;

use std::sync::Mutex;

use s2sdk_sys::generated as sys;

use crate::entity_json::Sb;
use crate::generated::pins;
use crate::ops::ident::{mem_ok, mod_iface, obj_is};
use crate::ops::OpErr;

use super::builders;

/// `GS_CACHE` (`mcp_bridge_game.inc:18`).
const GS_CACHE: usize = 192;
/// `GS_NAME_CAP` (`mcp_bridge_game.inc:244`): a copied name's bound, NUL included.
const GS_NAME_CAP: usize = 256;
/// `GS_PAGE_MASK` (`mcp_bridge_game.inc:246`): 4 KiB page granularity of the name-copy check.
const GS_PAGE_MASK: u64 = 0xFFF;
/// `GS_IDENT_CHUNK_MASK` (`mcp_bridge_game.inc:214`): `MAX_ENTITIES_IN_LIST - 1`, the identity
/// chunk's slot mask (512 is a power of two - the C static-asserts it, the generated const is
/// checked by the s2sdk layout tests).
pub(crate) const IDENT_CHUNK_MASK: u32 = sys::MAX_ENTITIES_IN_LIST - 1;

/// A copied C string from game memory: NUL-terminated, at most [`GS_NAME_CAP`] - 1 bytes (the
/// C's name-slot bytes; comparisons and `addjson` stop at the NUL).
#[derive(Clone, Copy)]
pub(crate) struct GsName {
    buf: [u8; GS_NAME_CAP],
}

impl GsName {
    fn new() -> Self {
        Self { buf: [0; GS_NAME_CAP] }
    }

    /// The bytes up to the NUL.
    pub fn as_bytes(&self) -> &[u8] {
        let len = self.buf.iter().position(|&b| b == 0).unwrap_or(GS_NAME_CAP);
        &self.buf[..len]
    }

    pub fn is_empty(&self) -> bool {
        self.buf[0] == 0
    }
}

/// `gs_copy_cstr` (`mcp_bridge_game.inc:249-260`): a C string copied from game memory,
/// probing one page boundary per page (`mem_ok`), stopping at the NUL or the cap.
fn gs_copy_cstr(src: *const u8, dst: &mut GsName) -> bool {
    if src.is_null() {
        return false;
    }
    for i in 0..GS_NAME_CAP - 1 {
        // SAFETY: `src + i` for both the probe and the read below; the probe is the C's
        // (`mcp_bridge_game.inc:252-254`) - one `mem_ok` per 4 KiB page, first byte always.
        let p = unsafe { src.add(i) };
        if (i == 0 || (p as u64) & GS_PAGE_MASK == 0) && !mem_ok(p.cast()) {
            dst.buf[0] = 0;
            return false;
        }
        // SAFETY: the probe above proved the page `p` is on readable, as the C's comment
        // promises (`mcp_bridge_game.inc:241`); one byte.
        dst.buf[i] = unsafe { *p };
        if dst.buf[i] == 0 {
            return true;
        }
    }
    dst.buf[GS_NAME_CAP - 1] = 0; // over-long: cut, still a readable name
    true
}

/// A page-checked copy for callers outside this module (e.g. schemaProbe's scope names).
pub(crate) fn copy_cstr_checked(src: *const u8) -> Option<GsName> {
    let mut name = GsName::new();
    gs_copy_cstr(src, &mut name).then_some(name)
}

/// The C's `seq(a, b)` over a copied C string: whole-byte C-string equality.
pub(crate) fn name_is(name: &GsName, other: &str) -> bool {
    name.as_bytes() == other.as_bytes()
}

/// The same equality against raw bytes (params are not always UTF-8; the C's `seq` never
/// converts).
pub(crate) fn name_is_bytes(name: &GsName, other: &[u8]) -> bool {
    name.as_bytes() == other
}

/// The C's `gs_starts` (`mcp_bridge_game.inc:790-793`) over a copied C string.
pub(crate) fn name_starts_with(name: &GsName, prefix: &[u8]) -> bool {
    name.as_bytes().starts_with(prefix)
}

/// The C's file statics (`mcp_bridge_game.inc:17`, `:18-21`, `:132`, `:215`, `:650`) in one
/// struct behind [`GS`].
pub(crate) struct Gs {
    /// `g_gsScope`: the client.dll type scope once up, as the raw object address (a pinned
    /// `CSchemaSystemTypeScope`, identity-checked at every store).
    scope: usize,
    /// `g_gsCache` / `g_gsCacheN`: resolved `(class, field) -> offset`, misses cached as -1.
    cache: [(&'static str, &'static str, i32); GS_CACHE],
    cache_n: usize,
    /// `g_gsLayout`: 0 unchecked, 1 ok, 2 sticky mismatch, 3 checked with nothing comparable.
    layout: u8,
    /// `g_gsLayoutMsg` (160 bytes, `mcp_bridge_game.inc:133`).
    layout_msg: [u8; 160],
    /// `g_gsIdentSize`: the schema's `CEntityIdentity` size, cached.
    ident_size: i32,
    /// `g_gsRulesIdx`: the cs_gamerules proxy's entindex, -1 when unknown.
    rules_idx: i32,
}

impl Gs {
    pub const fn new() -> Self {
        Self {
            scope: 0,
            cache: [("", "", 0); GS_CACHE],
            cache_n: 0,
            layout: 0,
            layout_msg: [0; 160],
            ident_size: 0,
            rules_idx: -1,
        }
    }

    /// Stores a layout mismatch exactly as [`gs_layout_check`] stores one (`mcp_bridge_game.inc:182`):
    /// the host tests' way to fabricate the sticky state; on the game the check itself runs.
    pub(crate) fn set_layout_mismatch(&mut self, msg: &[u8]) {
        self.layout_msg[..msg.len()].copy_from_slice(msg);
        self.layout = 2;
    }
}

/// The process-wide game-state statics. Locked for the whole body of every op that resolves
/// schema or entity data (uncontended: bodies run on the main thread's drain, one at a time).
pub static GS: Mutex<Gs> = Mutex::new(Gs::new());

/// `GS.lock()` with poisoning folded away (a body panic is answered `-32603` by the drain and
/// the state is plain memory, so a poisoned lock maps to "keep going with the same values").
pub fn lock() -> std::sync::MutexGuard<'static, Gs> {
    GS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The verified module base, or 0 - the read-side of the C's `g_modBase` under `mod_ok`.
pub(crate) fn modules_base(module: u8) -> u64 {
    crate::modules::live().base(module)
}

/// The C's `mod_ok` (`mcp_bridge.c:1156`): verified, based and pinned.
pub(crate) fn mod_ok(module: u8) -> bool {
    crate::modules::live().is_verified(module)
}

/// The stored scope as a typed pointer (only ever stored after `obj_is` passed).
fn scope_of(gs: &Gs) -> *mut sys::CSchemaSystemTypeScope {
    gs.scope as *mut sys::CSchemaSystemTypeScope
}

/// `gs_scope` (`mcp_bridge_game.inc:26-47`): the client.dll type scope, identity-checked, with
/// the F2 layout check run once on first success.
pub(crate) fn gs_scope(gs: &mut Gs, e: &mut OpErr) -> bool {
    if gs.scope != 0 {
        return true;
    }
    // mcp_bridge_game.inc:28-32
    let ss = mod_iface(pins::PIN_SCHEMA, sys::SCHEMASYSTEM_INTERFACE_VERSION);
    if !obj_is(ss, pins::PIN_SCHEMA, pins::SSV_SCHEMASYSTEM_VT.value as u32) {
        *e = OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "SchemaSystem_001 is not the pinned CSchemaSystem",
        );
        return false;
    }
    // FindTypeScopeForModule: the generated typed slot - the C's
    // S2_SLOT_ISchemaSystem_FindTypeScopeForModule, MSVC slot 13.
    let iface = ss.cast::<sys::ISchemaSystem>();
    // SAFETY: `obj_is` proved `ss` is a live object whose vptr is the pinned CSchemaSystem
    // vtable and whose pinned slots are unmodified; reading the typed vtable's entry and
    // calling it is what the C does unguarded (`mcp_bridge_game.inc:33`).
    let find_scope = unsafe {
        (*(*iface).vtable)
            .FindTypeScopeForModule
            .expect("ISchemaSystem::FindTypeScopeForModule (pinned slot 13)")
    };
    let module = b"client.dll\0";
    // SAFETY: as above; the arguments are a valid NUL-terminated literal and a null
    // out-pointer, the C's own (`mcp_bridge_game.inc:33`).
    let scope = unsafe {
        find_scope(
            iface,
            module.as_ptr().cast::<core::ffi::c_char>(),
            core::ptr::null_mut(),
        )
    };
    if !obj_is(
        scope.cast(),
        pins::PIN_SCHEMA,
        pins::SSV_TYPESCOPE_VT.value as u32,
    ) {
        *e = OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "client.dll type scope is not the pinned CSchemaSystemTypeScope",
        );
        return false;
    }
    // IsGlobalScope (bool in AL): the C reads it as uchar, the generated slot returns bool -
    // the same wire.
    let scope = scope.cast::<sys::CSchemaSystemTypeScope>();
    // SAFETY: `obj_is` proved the object and its pinned slots; the C calls the same slot
    // unguarded (`mcp_bridge_game.inc:38`).
    let is_global_fn = unsafe {
        (*(*scope).vtable)
            .base
            .IsGlobalScope
            .expect("ISchemaSystemTypeScope::IsGlobalScope (pinned slot 29)")
    };
    // SAFETY: the same verified entry, called with the interface pointer its generated
    // signature takes.
    let is_global = unsafe { is_global_fn(scope.cast::<sys::ISchemaSystemTypeScope>()) };
    if is_global {
        *e = OpErr::new(
            -32023,
            "SchemaUnavailable",
            "client.dll has not registered its schema scope yet",
        );
        return false;
    }
    gs.scope = scope as usize;
    // mcp_bridge_game.inc:42-45: the F2 layout cross-check before anything dereferences the
    // generated offsets; a mismatch rolls the scope back out.
    if !gs_layout_check(gs, e) {
        gs.scope = 0;
        return false;
    }
    true
}

/// `gs_class` (`mcp_bridge_game.inc:51-59`): the `SchemaClassInfoData_t` of `name` through the
/// scope's `FindRawClassBinding`, verified by reading the class's own `m_pName` back.
///
/// The C compares the name in place (`seq`); the read here is a bounded page-checked copy,
/// which answers identically on the live schema (static, valid memory) and refuses unreadable
/// memory instead of reading on.
pub(crate) fn gs_class(gs: &mut Gs, name: &'static str) -> *const sys::SchemaClassInfoData_t {
    if gs.scope == 0 {
        return core::ptr::null();
    }
    // FindRawClassBinding by name: the generated Windows vtable field is the C's MSVC slot
    // 25, the string overload (the `_1` twin is the by-class-id overload at slot 24).
    let scope = scope_of(gs);
    // SAFETY: `gs.scope` passed `obj_is` when it was stored (or no store happened); reading
    // the typed slot entry and calling it is the C's unguarded call
    // (`mcp_bridge_game.inc:54`).
    let find_raw = unsafe {
        (*(*scope).vtable)
            .base
            .FindRawClassBinding
            .expect("ISchemaSystemTypeScope::FindRawClassBinding (pinned slot 25)")
    };
    let mut name_buf = name.as_bytes().to_vec();
    name_buf.push(0);
    // SAFETY: as above; the argument is a NUL-terminated copy of `name` (the C passes the
    // caller's string literal), and `scope` is cast to the interface type the generated
    // signature takes.
    let cls = unsafe {
        find_raw(
            scope.cast::<sys::ISchemaSystemTypeScope>(),
            name_buf.as_ptr().cast::<core::ffi::c_char>(),
        )
    };
    if cls.is_null() || !mem_ok(cls.cast()) {
        return core::ptr::null();
    }
    let data = cls.cast::<sys::SchemaClassInfoData_t>();
    // SAFETY: `mem_ok` proved the class info readable; the name pointer is the generated
    // struct's member, read like the C (`mcp_bridge_game.inc:56`).
    let n = unsafe { addr_of!((*data).m_pName).read_unaligned() };
    if n.is_null() || !mem_ok(n.cast()) {
        return core::ptr::null();
    }
    let mut got = GsName::new();
    if !gs_copy_cstr(n.cast(), &mut got) || !name_is(&got, name) {
        return core::ptr::null();
    }
    data
}

/// `gs_field_in` (`mcp_bridge_game.inc:62-85`): the offset of `field` inside `cls`,
/// following single inheritance through `m_pBaseClasses[0..]` with base offsets added. Depth
/// is capped at 24 like the C.
fn gs_field_in(
    cls: *const sys::SchemaClassInfoData_t,
    field: &str,
    depth: i32,
    out: &mut i32,
) -> bool {
    if cls.is_null() || depth > 24 || !mem_ok(cls.cast()) {
        return false;
    }
    // SAFETY: `mem_ok` proved the class info readable; every member below is read with
    // `read_unaligned` from that same live allocation, exactly the C's reads
    // (`mcp_bridge_game.inc:64-82`).
    let nf = u32::from(unsafe { addr_of!((*cls).m_nNumFields).read_unaligned() });
    let fields = unsafe { addr_of!((*cls).m_pFields).read_unaligned() };
    if nf != 0 && !fields.is_null() && mem_ok(fields.cast()) {
        for i in 0..nf {
            // The stride is `size_of::<SchemaClassFieldData_t>()` - the generated struct the
            // C's S2_SIZEOF_SchemaClassFieldData_t comes from.
            let f = unsafe { fields.add(usize::try_from(i).expect("u32 to usize")) };
            // SAFETY: `f` is inside the (once) readable fields array; the name pointer is
            // probed with `mem_ok` before its bytes are read, as the C does (`:69-70`).
            let fname = unsafe { addr_of!((*f).m_pName).read_unaligned() };
            if !fname.is_null() && mem_ok(fname.cast()) {
                let mut got = GsName::new();
                if gs_copy_cstr(fname.cast(), &mut got) && name_is(&got, field) {
                    // SAFETY: the same live entry as above.
                    *out = unsafe { addr_of!((*f).m_nSingleInheritanceOffset).read_unaligned() };
                    return true;
                }
            }
        }
    }
    let nb = u32::from(unsafe { addr_of!((*cls).m_nNumBaseClasses).read_unaligned() });
    let bases = unsafe { addr_of!((*cls).m_pBaseClasses).read_unaligned() };
    if nb != 0 && !bases.is_null() && mem_ok(bases.cast()) {
        for j in 0..nb {
            // The stride is `size_of::<SchemaBaseClassInfoData_t>()` (the C's
            // S2_SIZEOF_SchemaBaseClassInfoData_t).
            let b = unsafe { bases.add(usize::try_from(j).expect("u32 to usize")) };
            // SAFETY: `b` is inside the (once) readable base array; the class pointer is
            // validated by the recursive call's `mem_ok`, as in the C (`:79-81`).
            let base_cls = unsafe { addr_of!((*b).m_pClass).read_unaligned() };
            let mut sub = 0;
            if gs_field_in(
                base_cls.cast::<sys::SchemaClassInfoData_t>(),
                field,
                depth + 1,
                &mut sub,
            ) {
                // SAFETY: the same live entry as above.
                let boff = unsafe { addr_of!((*b).m_nOffset).read_unaligned() };
                *out = boff as i32 + sub;
                return true;
            }
        }
    }
    false
}

/// `gs_off` (`mcp_bridge_game.inc:90-104`): the cached offset of `cls::field`; a miss is
/// cached too (off = -1) once the client scope is up.
pub(crate) fn gs_off(gs: &mut Gs, cls: &'static str, field: &'static str, out: &mut i32) -> bool {
    for entry in gs.cache.iter_mut().take(gs.cache_n) {
        if entry.0 == cls && entry.1 == field {
            if entry.2 < 0 {
                return false;
            }
            *out = entry.2;
            return true;
        }
    }
    let class = gs_class(gs, cls);
    let mut off = 0;
    let found = gs_field_in(class, field, 0, &mut off);
    // mcp_bridge_game.inc:99: scope not up yet - nothing is known, so nothing is cached.
    if !found && gs.scope == 0 {
        return false;
    }
    if gs.cache_n < GS_CACHE {
        gs.cache[gs.cache_n] = (cls, field, if found { off } else { -1 });
        gs.cache_n += 1;
    }
    if !found {
        return false;
    }
    *out = off;
    true
}

/// `gs_class_size` (`mcp_bridge_game.inc:105-110`).
pub(crate) fn gs_class_size(gs: &mut Gs, cls: &'static str, out: &mut i32) -> bool {
    let c = gs_class(gs, cls);
    if c.is_null() {
        return false;
    }
    // SAFETY: `gs_class` proved the class info readable.
    let size = unsafe { addr_of!((*c).m_nSizeOf).read_unaligned() };
    *out = size;
    *out > 0
}

/// The F2 mismatch message, size form (`mcp_bridge_game.inc:168-172`).
pub(crate) fn layout_mismatch_size(out: &mut Sb<'_>, schema_size: i64, header_size: i64) {
    out.add(b"CEntityIdentity layout mismatch: schema ");
    out.addi(schema_size);
    out.add(b" vs header ");
    out.addi(header_size);
    out.add(b" size");
    out.add(b" (update s2sdk upstream)");
}

/// The F2 mismatch message, anchor form (`mcp_bridge_game.inc:173-179`).
pub(crate) fn layout_mismatch_anchor(out: &mut Sb<'_>, field: &[u8], schema_off: i64, header_off: i64) {
    out.add(b"CEntityIdentity layout mismatch: schema ");
    out.add(field);
    out.add(b" ");
    out.addi(schema_off);
    out.add(b" vs header ");
    out.addi(header_off);
    out.add(b" offset");
    out.add(b" (update s2sdk upstream)");
}

/// The bytes of a NUL-terminated scratch buffer, up to the NUL.
fn cstr_bytes(buf: &[u8]) -> &[u8] {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..len]
}

fn layout_err(msg: &[u8]) -> OpErr {
    OpErr::new(-32023, "SchemaUnavailable", &String::from_utf8_lossy(msg))
}

/// One anchor row of [`gs_layout_check`]: the field's name, the header offset (the generated
/// struct's own) and the offset the schema resolved (filled by the caller; an unregistered
/// field keeps the header offset, which never mismatches).
pub(crate) type LayoutAnchors = [(&'static str, i32, i32); 4];

/// `gs_layout_check`'s entry over stored state (`mcp_bridge_game.inc:138-139`): a stored
/// mismatch is sticky - it re-answers its own message and refuses; a state already checked
/// (1 or 3) passes without re-running; `None` means the schema reads must run. Pure, so the
/// host tests drive the sticky state.
pub(crate) fn layout_entry(layout: u8, stored: &[u8], e: &mut OpErr) -> Option<bool> {
    if layout == 2 {
        *e = layout_err(stored);
        return Some(false);
    }
    if layout != 0 {
        return Some(true);
    }
    None
}

/// `gs_layout_check`'s decision (`mcp_bridge_game.inc:149-184`), pure over the schema's
/// reads: `Ok` is the layout state to store (1 when the evidence agrees, 3 when the schema
/// gave nothing comparable - neither the class nor any anchor); `Err` is the mismatch
/// message (the size form wins over the anchor form, and the anchor reported is the last
/// mismatching one, the C's scan order).
pub(crate) fn layout_decide(
    have_size: bool,
    szs: i32,
    anchors: &LayoutAnchors,
    resolved: usize,
    header_size: i64,
) -> Result<u8, String> {
    let bad_size = have_size && i64::from(szs) != header_size;
    let mut bad_anchor: i32 = -1;
    for (i, anchor) in anchors.iter().enumerate() {
        // The schema not registering a field is fine (the C's banner, `:126`): skip, don't
        // fail (`:153`).
        if anchor.2 != anchor.1 {
            bad_anchor = i as i32;
        }
    }
    if !bad_size && bad_anchor < 0 && (have_size || resolved > 0) {
        return Ok(1); // whichever the schema gave us, upstream matches it (`:158`)
    }
    if !bad_size && bad_anchor < 0 {
        return Ok(3); // schema had neither this class nor the anchors (`:161-163`)
    }
    // mcp_bridge_game.inc:165-181: the message, built in the C's 160 bytes.
    let mut buf = [0u8; 160];
    let mut m = Sb::new(&mut buf);
    if bad_size {
        layout_mismatch_size(&mut m, i64::from(szs), header_size);
    } else {
        let a = anchors[bad_anchor as usize];
        layout_mismatch_anchor(&mut m, a.0.as_bytes(), i64::from(a.2), i64::from(a.1));
    }
    Err(String::from_utf8_lossy(m.as_cstr()).into_owned())
}

/// `gs_layout_check` (`mcp_bridge_game.inc:137-185`): prove the compiled `CEntityIdentity`
/// against the live schema (size + every anchor field the schema registers), once; a mismatch
/// is sticky and fails every entity-resolving op closed.
pub(crate) fn gs_layout_check(gs: &mut Gs, e: &mut OpErr) -> bool {
    if let Some(done) = layout_entry(gs.layout, cstr_bytes(&gs.layout_msg), e) {
        return done;
    }
    let mut szs = 0;
    let have_size = gs_class_size(gs, "CEntityIdentity", &mut szs);
    // The anchors are the generated leading-block fields the C names (`:143-148`, gen 20, 24,
    // 32, 48): the generated struct's own offsets, never literals here.
    let mut anchors: LayoutAnchors = [
        (
            "m_nameStringTableIndex",
            offset_of!(sys::CEntityIdentity, m_nameStringTableIndex) as i32,
            0,
        ),
        ("m_name", offset_of!(sys::CEntityIdentity, m_name) as i32, 0),
        (
            "m_designerName",
            offset_of!(sys::CEntityIdentity, m_designerName) as i32,
            0,
        ),
        ("m_flags", offset_of!(sys::CEntityIdentity, m_flags) as i32, 0),
    ];
    let header_size = core::mem::size_of::<sys::CEntityIdentity>() as i64;
    let mut resolved = 0;
    for anchor in anchors.iter_mut() {
        let mut off = 0;
        if gs_off(gs, "CEntityIdentity", anchor.0, &mut off) {
            resolved += 1;
            anchor.2 = off;
        } else {
            anchor.2 = anchor.1;
        }
    }
    match layout_decide(have_size, szs, &anchors, resolved, header_size) {
        Ok(layout) => {
            gs.layout = layout;
            true
        }
        Err(msg) => {
            // mcp_bridge_game.inc:182-183: store the message; the state is sticky from here.
            gs.set_layout_mismatch(msg.as_bytes());
            *e = layout_err(cstr_bytes(&gs.layout_msg));
            false
        }
    }
}

/// `gs_rd` (`mcp_bridge_game.inc:188-195`): a typed read of `n` bytes at a schema-resolved
/// offset; every failure is reported, never guessed.
pub(crate) fn gs_rd(gs: &mut Gs, ent: *const c_void, cls: &'static str, field: &'static str, dst: &mut [u8]) -> bool {
    if ent.is_null() {
        return false;
    }
    let mut off = 0;
    if !gs_off(gs, cls, field, &mut off) {
        return false;
    }
    let n = dst.len();
    // `p` is the schema-resolved field address inside the caller's entity - wrapping
    // arithmetic, the C's `(u8 *)ent + off` with any i32 offset (`mcp_bridge_game.inc:191`);
    // the two probes below are the C's own guard (`:192`).
    let p = ent.cast::<u8>().wrapping_byte_offset(off as isize);
    if !mem_ok(p.cast()) || !mem_ok(unsafe { p.add(n - 1) }.cast()) {
        return false;
    }
    for (i, d) in dst.iter_mut().enumerate() {
        // SAFETY: the probes above covered the first and last byte of this range; the C
        // copies the whole range after the same two probes (`:193`).
        *d = unsafe { *p.add(i) };
    }
    true
}

/// `gs_i32` (`mcp_bridge_game.inc:196`).
pub(crate) fn gs_i32(gs: &mut Gs, ent: *const c_void, c: &'static str, f: &'static str, v: &mut i32) -> bool {
    let mut b = [0u8; 4];
    if gs_rd(gs, ent, c, f, &mut b) {
        *v = i32::from_le_bytes(b);
        true
    } else {
        false
    }
}

/// `gs_u32` (`mcp_bridge_game.inc:197`).
pub(crate) fn gs_u32(gs: &mut Gs, ent: *const c_void, c: &'static str, f: &'static str, v: &mut u32) -> bool {
    let mut b = [0u8; 4];
    if gs_rd(gs, ent, c, f, &mut b) {
        *v = u32::from_le_bytes(b);
        true
    } else {
        false
    }
}

/// `gs_u8` (`mcp_bridge_game.inc:198`).
pub(crate) fn gs_u8(gs: &mut Gs, ent: *const c_void, c: &'static str, f: &'static str, v: &mut u8) -> bool {
    gs_rd(gs, ent, c, f, core::slice::from_mut(v))
}

/// `gs_f32` (`mcp_bridge_game.inc:199`).
pub(crate) fn gs_f32(gs: &mut Gs, ent: *const c_void, c: &'static str, f: &'static str, v: &mut f32) -> bool {
    let mut b = [0u8; 4];
    if gs_rd(gs, ent, c, f, &mut b) {
        *v = f32::from_le_bytes(b);
        true
    } else {
        false
    }
}

/// `gs_vec` (`mcp_bridge_game.inc:200`): 12 bytes as three floats.
pub(crate) fn gs_vec(gs: &mut Gs, ent: *const c_void, c: &'static str, f: &'static str, v: &mut [f32; 3]) -> bool {
    let mut b = [0u8; 12];
    if gs_rd(gs, ent, c, f, &mut b) {
        *v = [
            f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            f32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            f32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        ];
        true
    } else {
        false
    }
}

/// `gs_ptr` (`mcp_bridge_game.inc:201`): 8 bytes as a pointer.
pub(crate) fn gs_ptr(gs: &mut Gs, ent: *const c_void, c: &'static str, f: &'static str, v: &mut *mut c_void) -> bool {
    let mut b = [0u8; 8];
    if gs_rd(gs, ent, c, f, &mut b) {
        *v = usize::from_le_bytes(b) as *mut c_void;
        true
    } else {
        false
    }
}

/// `gs_ident` (`mcp_bridge_game.inc:216-224`): the entity identity at `index` through the
/// client's chunk table.
pub(crate) fn gs_ident(gs: &mut Gs, index: u32) -> *mut u8 {
    // mcp_bridge_game.inc:217: the identity index bound (1 << ENTITY_INDEX_BITS, generated).
    if index >= 1u32 << sys::ENTITY_INDEX_BITS {
        return core::ptr::null_mut();
    }
    if gs.ident_size == 0 {
        let mut size = 0;
        if !gs_class_size(gs, "CEntityIdentity", &mut size) {
            return core::ptr::null_mut();
        }
        gs.ident_size = size;
    }
    let base = modules_base(pins::PIN_CLIENT);
    if base == 0 {
        return core::ptr::null_mut();
    }
    // mcp_bridge_game.inc:219: `chunks = *(u8***)(base + CG_ENTITY_CHUNKS)`.
    // SAFETY: `base` is a verified module's load address (the C's `g_modBase[PIN_CLIENT]`,
    // only read when the module verified), so the global and the word it holds are inside the
    // pinned image the module gate checked byte-for-byte; the C reads it unguarded (`:219`).
    let chunks =
        unsafe { ((base + pins::CG_ENTITY_CHUNKS.value) as *const *mut *mut u8).read_unaligned() };
    if chunks.is_null() || !mem_ok(chunks.cast()) {
        return core::ptr::null_mut();
    }
    // SAFETY: `chunks` passed `mem_ok`; the entry index is bounded by the chunk count the
    // index bound implies, the C's own arithmetic (`mcp_bridge_game.inc:221`).
    let chunk = unsafe {
        *(chunks.add(usize::try_from(index / sys::MAX_ENTITIES_IN_LIST).expect("u32 to usize")))
    };
    if chunk.is_null() || !mem_ok(chunk.cast()) {
        return core::ptr::null_mut();
    }
    // SAFETY: `chunk` passed `mem_ok`; the slot offset is the schema-measured identity size
    // times the in-chunk mask, exactly the C's (`mcp_bridge_game.inc:223`).
    unsafe {
        chunk.add(
            usize::try_from(gs.ident_size).expect("positive schema size")
                * usize::try_from(index & IDENT_CHUNK_MASK).expect("u32 to usize"),
        )
    }
}

/// `gs_ent_by_index` (`mcp_bridge_game.inc:225-230`).
pub(crate) fn gs_ent_by_index(gs: &mut Gs, index: u32) -> *mut c_void {
    let id = gs_ident(gs, index);
    if id.is_null() || !mem_ok(id.cast()) {
        return core::ptr::null_mut();
    }
    // SAFETY: `mem_ok` proved the identity readable; `m_pInstance` is the generated struct's
    // first member (the C's S2_OFF_CEntityIdentity_m_pInstance), read like the C (`:228`).
    let ent = unsafe { addr_of!((*id.cast::<sys::CEntityIdentity>()).m_pInstance).read_unaligned() };
    if !ent.is_null() && mem_ok(ent.cast()) {
        ent.cast()
    } else {
        core::ptr::null_mut()
    }
}

/// `gs_ent_by_handle` (`mcp_bridge_game.inc:233-239`): the entity a `CHandle` names, proven by
/// the identity's own `m_EHandle`.
pub(crate) fn gs_ent_by_handle(gs: &mut Gs, h: u32) -> *mut c_void {
    if h == sys::INVALID_EHANDLE_INDEX {
        return core::ptr::null_mut();
    }
    let id = gs_ident(gs, h & sys::ENTITY_INDEX_MASK);
    if id.is_null() || !mem_ok(id.cast()) {
        return core::ptr::null_mut();
    }
    // SAFETY: `mem_ok` proved the identity readable; `m_EHandle` is the generated struct's
    // third member (the C's S2_OFF_CEntityIdentity_m_EHandle), read like the C (`:236`).
    let got = unsafe {
        addr_of!((*id.cast::<sys::CEntityIdentity>()).m_EHandle)
            .cast::<u32>()
            .read_unaligned()
    };
    if got != h {
        return core::ptr::null_mut();
    }
    // SAFETY: the same readable identity as above.
    let ent = unsafe { addr_of!((*id.cast::<sys::CEntityIdentity>()).m_pInstance).read_unaligned() };
    if !ent.is_null() && mem_ok(ent.cast()) {
        ent.cast()
    } else {
        core::ptr::null_mut()
    }
}

/// The `CUtlSymbolLarge` char* at `off` inside a readable identity (the C reads
/// `*(const char**)(id + off + S2_OFF_CUtlSymbolLarge_m_pString)`, `mcp_bridge_game.inc:272`,
/// `:294`).
///
/// # Safety
///
/// `id` must have passed `mem_ok` and `off` must be a schema-resolved offset inside it; the
/// symbol pointer's own bytes are read with the same guard the C uses (one unguarded word
/// read, then the copy probes page by page).
unsafe fn symbol_string(id: *mut u8, off: i32) -> *const u8 {
    unsafe {
        // Wrapping arithmetic, the C's `*(const char **)(id + off + S2_OFF_CUtlSymbolLarge_m_pString)`
        // with any i32 offset (`mcp_bridge_game.inc:272`, `:294`); the symbol pointer's bytes
        // are guarded where the C guards them (see below).
        id.wrapping_byte_offset(off as isize + offset_of!(sys::CUtlSymbolLarge, m_pString) as isize)
            .cast::<*const u8>()
            .read_unaligned()
    }
}

/// `gs_designer_name` (`mcp_bridge_game.inc:266-273`): the identity's `m_designerName`
/// (CUtlSymbolLarge) copied into a bounded slot.
pub(crate) fn gs_designer_name(gs: &mut Gs, index: u32) -> Option<GsName> {
    let id = gs_ident(gs, index);
    if id.is_null() || !mem_ok(id.cast()) {
        return None;
    }
    let mut off = 0;
    if !gs_off(gs, "CEntityIdentity", "m_designerName", &mut off) {
        return None;
    }
    let mut name = GsName::new();
    // SAFETY: `mem_ok` proved the identity readable and `off` a schema-resolved offset inside
    // it; the symbol's char* is probed page by page by the copy - the C's sequence
    // (`mcp_bridge_game.inc:269-272`).
    let sym = unsafe { symbol_string(id, off) };
    gs_copy_cstr(sym, &mut name).then_some(name)
}

/// `gs_entity_name` (`mcp_bridge_game.inc:289-296`): the identity's `m_name`; empty = none.
pub(crate) fn gs_entity_name(gs: &mut Gs, index: u32) -> Option<GsName> {
    let id = gs_ident(gs, index);
    if id.is_null() || !mem_ok(id.cast()) {
        return None;
    }
    let mut off = 0;
    if !gs_off(gs, "CEntityIdentity", "m_name", &mut off) {
        return None;
    }
    let mut name = GsName::new();
    // SAFETY: as `gs_designer_name` - a schema-resolved offset inside a readable identity.
    let sym = unsafe { symbol_string(id, off) };
    if gs_copy_cstr(sym, &mut name) && !name.is_empty() {
        Some(name)
    } else {
        None
    }
}

/// `gs_ent_index` (`mcp_bridge_game.inc:274-278`): a controller/pawn entity's entindex from
/// its identity's `m_EHandle`.
pub(crate) fn gs_ent_index(ent: *const c_void) -> i32 {
    if ent.is_null() {
        return -1;
    }
    // SAFETY: the caller's identity check (the controller from the verified local-controller
    // table, or a handle proven by `gs_ent_by_handle`) made `ent` a live CEntityInstance;
    // `m_pEntity` is the generated member the C spells S2_OFF_CEntityInstance_m_pEntity, read
    // like the C (`mcp_bridge_game.inc:275`).
    let id = unsafe { addr_of!((*ent.cast::<sys::CEntityInstance>()).m_pEntity).read_unaligned() };
    if id.is_null() || !mem_ok(id.cast()) {
        return -1;
    }
    // SAFETY: `mem_ok` proved the identity readable.
    let handle = unsafe {
        addr_of!((*id.cast::<sys::CEntityIdentity>()).m_EHandle)
            .cast::<u32>()
            .read_unaligned()
    };
    (handle & sys::ENTITY_INDEX_MASK) as i32
}

/// `gs_origin` (`mcp_bridge_game.inc:279-283`).
pub(crate) fn gs_origin(gs: &mut Gs, ent: *const c_void, out: &mut [f32; 3]) -> bool {
    let mut node: *mut c_void = core::ptr::null_mut();
    if !gs_ptr(gs, ent, "C_BaseEntity", "m_pGameSceneNode", &mut node)
        || node.is_null()
        || !mem_ok(node)
    {
        return false;
    }
    gs_vec(gs, node, "CGameSceneNode", "m_vecAbsOrigin", out)
}

/// `gs_angles` (`mcp_bridge_game.inc:284-288`).
pub(crate) fn gs_angles(gs: &mut Gs, ent: *const c_void, out: &mut [f32; 3]) -> bool {
    let mut node: *mut c_void = core::ptr::null_mut();
    if !gs_ptr(gs, ent, "C_BaseEntity", "m_pGameSceneNode", &mut node)
        || node.is_null()
        || !mem_ok(node)
    {
        return false;
    }
    gs_vec(gs, node, "CGameSceneNode", "m_angAbsRotation", out)
}

/// `gs_cstr` for the player name (the C hands the inline `char[]` in place,
/// `mcp_bridge_game.inc:437`): a bounded page-checked copy, which `addjson` renders
/// identically (both stop at the NUL).
pub(crate) fn gs_inline_cstr(
    gs: &mut Gs,
    ent: *const c_void,
    cls: &'static str,
    field: &'static str,
) -> Option<GsName> {
    if ent.is_null() {
        return None;
    }
    let mut off = 0;
    if !gs_off(gs, cls, field, &mut off) {
        return None;
    }
    // The entity's readability is the caller's identity check (the controller came from the
    // verified local-controller table or a proven handle); the field offset is
    // schema-resolved, the copy probes every page it touches. Wrapping arithmetic, the C's
    // `(const char *)ent + off` with any i32 offset (`mcp_bridge_game.inc:205`).
    let s = ent.cast::<u8>().wrapping_byte_offset(off as isize);
    copy_cstr_checked(s)
}

/// `gs_engine_client` (`mcp_bridge_game.inc:303-310`): the engine client interface,
/// identity-checked.
pub(crate) fn gs_engine_client() -> Result<*mut c_void, OpErr> {
    let ec = mod_iface(pins::PIN_ENGINE2, sys::SOURCE2ENGINETOCLIENT_INTERFACE_VERSION);
    if !obj_is(ec, pins::PIN_ENGINE2, pins::EV_ENGINECLIENT_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "Source2EngineToClient001 is not the pinned CEngineClient",
        ));
    }
    Ok(ec)
}

/// `gs_local_controller` (`mcp_bridge_game.inc:313-321`): `CG_LOCAL_CONTROLLERS[0]`,
/// cross-checked against GetLocalPlayer(0)+1. `Err` is the C's `why` string.
pub(crate) fn gs_local_controller(
    ec: *const c_void,
    player_slot: Option<&mut i32>,
) -> Result<*mut c_void, &'static str> {
    let iface = ec.cast_mut().cast::<sys::IVEngineClient2>();
    // The generated vtable field's shape is per target: on Windows the real slot takes the
    // MSVC hidden-return pointer for the class return type CPlayerSlot
    // (`s2sdk-sys/src/generated/engine.rs`, the cfg(windows) vtable struct - the same shape
    // the C's typedef comment pins, `mcp_bridge_game.inc:299`), and elsewhere the header's
    // by-value form, which that comment prescribes for a Linux port. Either way it is the
    // generated typed field at the pinned slot 34 - no raw fn pointer.
    #[cfg(windows)]
    let ps: i32 = {
        // SAFETY: `obj_is` proved `ec` is the pinned CEngineClient, so its vptr is the pinned
        // vtable and the typed field's entry its verified slot-34 function; the C calls the
        // same slot unguarded (`mcp_bridge_game.inc:315`).
        let get_local = unsafe {
            (*(*iface).vtable)
                .GetLocalPlayer
                .expect("IVEngineClient2::GetLocalPlayer (pinned slot 34)")
        };
        // SAFETY: the verified slot entry; arguments (ret, this, slot 0) - the MSVC hidden
        // return pointer, the same out slot the C passes `&ps` to (`mcp_bridge_game.inc:315`).
        // -1 is the C's `int ps = -1` default, kept for a slot that never writes (`:314`).
        let mut slot = sys::CPlayerSlot { m_Data: -1 };
        unsafe { get_local(iface, core::ptr::from_mut(&mut slot), sys::CSplitScreenSlot { m_Data: 0 }) };
        slot.m_Data
    };
    #[cfg(not(windows))]
    let ps: i32 = {
        // SAFETY: `obj_is` proved `ec` is the pinned CEngineClient, so its vptr is the pinned
        // vtable and the typed field's entry its verified slot-34 function; the C calls the
        // same slot unguarded (`mcp_bridge_game.inc:315`).
        let get_local = unsafe {
            (*(*iface).vtable)
                .GetLocalPlayer
                .expect("IVEngineClient2::GetLocalPlayer (pinned slot 34)")
        };
        // SAFETY: the verified slot entry; the header's by-value return shape, slot 0.
        let slot = unsafe { get_local(iface, sys::CSplitScreenSlot { m_Data: 0 }) };
        slot.m_Data
    };
    if let Some(slot) = player_slot {
        *slot = ps;
    }
    let base = modules_base(pins::PIN_CLIENT);
    if base == 0 {
        return Err("no local controller (not in a game?)");
    }
    // SAFETY: `base` is a verified module's load address; the global is inside the pinned
    // image, the C reads it unguarded (`mcp_bridge_game.inc:317`).
    let ctl =
        unsafe { ((base + pins::CG_LOCAL_CONTROLLERS.value) as *const *mut c_void).read_unaligned() };
    if ctl.is_null() || !mem_ok(ctl) {
        return Err("no local controller (not in a game?)");
    }
    if ps >= 0 && gs_ent_index(ctl) != ps + 1 {
        return Err("local controller table disagrees with GetLocalPlayer");
    }
    Ok(ctl)
}

/// `gs_weapon_services` (`mcp_bridge_game.inc:396-400`).
pub(crate) fn gs_weapon_services(gs: &mut Gs, pawn: *const c_void) -> *mut c_void {
    let mut ws: *mut c_void = core::ptr::null_mut();
    if !gs_ptr(gs, pawn, "C_BasePlayerPawn", "m_pWeaponServices", &mut ws)
        || ws.is_null()
        || !mem_ok(ws)
    {
        return core::ptr::null_mut();
    }
    ws
}

/// `gs_active_weapon` (`mcp_bridge_game.inc:401-406`).
pub(crate) fn gs_active_weapon(gs: &mut Gs, pawn: *const c_void) -> *mut c_void {
    let ws = gs_weapon_services(gs, pawn);
    let mut h = sys::INVALID_EHANDLE_INDEX;
    if ws.is_null() || !gs_u32(gs, ws, "CPlayer_WeaponServices", "m_hActiveWeapon", &mut h) {
        return core::ptr::null_mut();
    }
    gs_ent_by_handle(gs, h)
}

/// The reads of `gs_weapon_json` (`mcp_bridge_game.inc:368-393`) as a plain struct, handed to
/// [`builders::weapon_json`].
pub(crate) fn read_weapon(gs: &mut Gs, w: *const c_void) -> builders::WeaponJson {
    let mut clip1 = -1;
    let mut clip2 = -1;
    let mut reserve = -1;
    let mut def = -1;
    let mut handle = sys::INVALID_EHANDLE_INDEX;
    let _ = gs_i32(gs, w, "C_BasePlayerWeapon", "m_iClip1", &mut clip1);
    let _ = gs_i32(gs, w, "C_BasePlayerWeapon", "m_iClip2", &mut clip2);
    // mcp_bridge_game.inc:373: the first entry of the int[2] m_pReserveAmmo.
    let mut rv = [0u8; 8];
    if gs_rd(gs, w, "C_BasePlayerWeapon", "m_pReserveAmmo", &mut rv) {
        reserve = i32::from_le_bytes([rv[0], rv[1], rv[2], rv[3]]);
    }
    // mcp_bridge_game.inc:374-378: the attribute chain to m_iItemDefinitionIndex (u16).
    {
        let mut a = 0;
        let mut b = 0;
        if gs_off(gs, "C_EconEntity", "m_AttributeManager", &mut a)
            && gs_off(gs, "C_AttributeContainer", "m_Item", &mut b)
        {
            // `w` is the caller's readable weapon object; the two offsets are
            // schema-resolved inside it, the C's `(u8*)w + a + b` (`mcp_bridge_game.inc:376`)
            // - wrapping, so a negative offset behaves as the C's.
            let item = w.cast::<u8>().wrapping_byte_offset(a as isize + b as isize);
            let mut d16 = [0u8; 2];
            if gs_rd(gs, item.cast::<c_void>(), "C_EconItemView", "m_iItemDefinitionIndex", &mut d16) {
                def = i32::from(u16::from_le_bytes(d16));
            }
        }
    }
    let idx = gs_ent_index(w);
    let mut name = None;
    if idx >= 0 {
        let index = u32::try_from(idx).expect("checked >= 0");
        let id = gs_ident(gs, index);
        if !id.is_null() && mem_ok(id.cast()) {
            // SAFETY: the readable identity's m_EHandle, the C's read
            // (`mcp_bridge_game.inc:381`).
            handle = unsafe {
                addr_of!((*id.cast::<sys::CEntityIdentity>()).m_EHandle)
                    .cast::<u32>()
                    .read_unaligned()
            };
        }
        // mcp_bridge_game.inc:386: `idx >= 0 ? gs_designer_name((u32)idx) : 0` - an empty
        // name stays `""` (only a failed copy is null).
        name = gs_designer_name(gs, index).map(|n| n.as_bytes().to_vec());
    }
    let mut reloading = 0u8;
    let _ = gs_u8(gs, w, "C_CSWeaponBase", "m_bInReload", &mut reloading);
    builders::WeaponJson {
        entindex: idx,
        handle,
        name,
        def_index: def,
        clip1,
        clip2,
        reserve,
        reloading: reloading != 0,
    }
}

/// `gs_weapon_json` (`mcp_bridge_game.inc:368-393`): the reads, then the pure builder.
pub(crate) fn gs_weapon_json(gs: &mut Gs, w: *const c_void, out: &mut Sb<'_>) {
    let w = read_weapon(gs, w);
    builders::weapon_json(&w, out);
}

/// `gs_session_header` (`mcp_bridge_game.inc:475-484`): the engine's tick, in-game flag and
/// short level name, then the pure builder.
pub(crate) fn gs_session_header(ec: *const c_void, out: &mut Sb<'_>) {
    let iface = ec.cast_mut().cast::<sys::IVEngineClient2>();
    // SAFETY: `gs_engine_client` proved the object is the pinned CEngineClient (`obj_is`);
    // the three typed slots are the C's pinned calls (`mcp_bridge_game.inc:476-478`).
    let (get_tick, in_game_fn, level_name_fn) = unsafe {
        let vt = &*(*iface).vtable;
        (
            vt.GetLastServerTick.expect("IVEngineClient2::GetLastServerTick (pinned slot 37)"),
            vt.IsInGame.expect("IVEngineClient2::IsInGame (pinned slot 39)"),
            vt.GetLevelNameShort.expect("IVEngineClient2::GetLevelNameShort (pinned slot 65)"),
        )
    };
    // SAFETY: the verified entries, called with the object they were read from.
    let (tick, in_game, map) =
        unsafe { (get_tick(iface), in_game_fn(iface), level_name_fn(iface)) };
    // mcp_bridge_game.inc:482: the map is null when unreadable or empty.
    let mut map_name = None;
    if !map.is_null() && mem_ok(map.cast()) {
        map_name = copy_cstr_checked(map.cast());
    }
    let map = map_name.filter(|m| !m.is_empty()).map(|m| m.as_bytes().to_vec());
    builders::session_header(out, tick, in_game, map.as_deref());
}

/// `gs_gamerules_ent` (`mcp_bridge_game.inc:651-670`): the cs_gamerules proxy, its entindex
/// cached and re-validated by designer name on each call, like the radar's world walk.
pub(crate) fn gs_gamerules_ent(gs: &mut Gs, idx_out: &mut i32) -> *mut c_void {
    // mcp_bridge_game.inc:652-657: trust the cache only while the identity agrees with it.
    if gs.rules_idx >= 0 {
        let idx = gs.rules_idx as u32;
        let dn = gs_designer_name(gs, idx);
        let ent = gs_ent_by_index(gs, idx);
        if dn.as_ref().is_some_and(|d| name_is(d, "cs_gamerules"))
            && !ent.is_null()
            && gs_ent_index(ent) == gs.rules_idx
        {
            *idx_out = gs.rules_idx;
            return ent;
        }
        gs.rules_idx = -1;
    }
    // mcp_bridge_game.inc:658-668: the world scan; chunk-null skips 512 at a time.
    let mut i: u32 = 1;
    while i < sys::MAX_EDICTS {
        let id = gs_ident(gs, i);
        if id.is_null() {
            if i & IDENT_CHUNK_MASK == 0 && gs_ident(gs, i | IDENT_CHUNK_MASK).is_null() {
                i |= IDENT_CHUNK_MASK;
            }
            i += 1;
            continue;
        }
        let Some(dn) = gs_designer_name(gs, i) else {
            i += 1;
            continue;
        };
        if !name_is(&dn, "cs_gamerules") {
            i += 1;
            continue;
        }
        let ent = gs_ent_by_index(gs, i);
        if ent.is_null() {
            i += 1;
            continue;
        }
        gs.rules_idx = i as i32;
        *idx_out = i as i32;
        return ent;
    }
    core::ptr::null_mut()
}
