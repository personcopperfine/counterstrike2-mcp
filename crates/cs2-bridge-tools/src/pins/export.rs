//! The JSON document `sigs/scripts/pinsdata.py` prints: every module's identity and every
//! pin-relevant signature entry, already resolved and gated. The field meanings are that
//! script's docstring; this side only reads them.

use serde::Deserialize;

/// The whole export: the game build it was checked against and both platforms' modules.
#[derive(Debug, Deserialize)]
pub struct Export {
    /// `sigs/CURRENT_BUILD` as sigcheck.py read it (builds[]/expect[] were checked against it).
    pub build: String,
    /// The Windows DLLs, in the order they were requested (the PIN_* index order).
    pub windows: Vec<Module>,
    /// The linux modules (`sigs/signatures/lib*.json` with platform linux), sorted by file.
    pub linux: Vec<Module>,
}

/// One module: its identity and its entries that carry `bridge`, `sdk` or `ident_slot`.
#[derive(Debug, Deserialize)]
pub struct Module {
    /// The module's file name (`hammer.dll`, `libclient.so`).
    pub module: String,
    /// Whole-file SHA-256, hex.
    pub sha256: String,
    /// PE TimeDateStamp (0 for an ELF).
    pub time_stamp: u32,
    /// PE SizeOfImage, or the highest section end of an ELF.
    pub size_of_image: u64,
    /// PE optional-header CheckSum (Windows only).
    #[serde(default)]
    pub check_sum: u32,
    /// RSDS guid, hex (Windows only; absent when the image has no CodeView record).
    #[serde(default)]
    pub rsds_guid: Option<String>,
    /// RSDS age (Windows only).
    #[serde(default)]
    pub rsds_age: u32,
    /// The entries, in the database's JSON order.
    pub entries: Vec<Entry>,
}

/// One signature entry, as far as the pins need it.
#[derive(Debug, Deserialize)]
pub struct Entry {
    /// The entry's key in `entries` (not necessarily its bridge name).
    pub name: String,
    /// `vtable`, `function`, `global`, `offset`, `const`, `string`.
    pub kind: String,
    /// The name the bridge binds it by (`None` for an `sdk`-only or `ident_slot`-only entry).
    pub bridge: Option<String>,
    /// The locator, when the entry has one (a literal `value` entry has none).
    pub locator: Option<Locator>,
    /// Free text; the doc comment of a vslot/const/function/global pin.
    pub notes: Option<String>,
    /// Whether the generated SDK header owns the number (`sdk` field present).
    pub sdk: bool,
    /// `kind=vtable` only: the slot checked for identity.
    pub ident_slot: Option<u64>,
    /// What the locator resolved to: an RVA or an integer (`None` when it was not resolved, i.e.
    /// neither `bridge` nor `sdk`).
    pub value: Option<u64>,
    /// The byte window at an RVA-valued bridge entry (function, global, vslot target).
    pub window: Option<Window>,
    /// The function `ident_slot` holds (`None`: not an image pointer, or no slot).
    pub ident_target: Option<u64>,
}

/// The locator fields the section mapping reads (the rest of the locator is ignored).
#[derive(Debug, Deserialize)]
pub struct Locator {
    /// The locator type (`rtti`, `vslot`, `sig`, ...).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// `rtti`: the MSVC RTTI class name.
    pub class: Option<String>,
    /// `itanium_vtable`: the `_ZTV` symbol.
    pub symbol: Option<String>,
    /// `vslot`: the vtable entry whose slot is read.
    pub vtable: Option<String>,
    /// `vslot`: the slot index.
    pub slot: Option<u64>,
}

/// The pinned byte window at an RVA.
#[derive(Debug, Deserialize)]
pub struct Window {
    /// The 24 bytes there, hex; `None` when the RVA is in no section.
    pub bytes: Option<String>,
    /// Relocated addresses inside the window (the loader rewrites those bytes).
    pub relocs: Vec<u64>,
}
