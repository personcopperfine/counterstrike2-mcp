//! Error and result types for the safe layer.
//!
//! Every fallible call maps onto one of these variants; the vtable dispatch helper convention
//! (`ok_or(Error::MissingSlot(..))`) is documented in [`crate::interface`].

use s2sdk_sys::generated as sys;

use thiserror::Error as ThisError;

/// Errors returned by the safe layer.
#[derive(Debug, ThisError)]
pub enum Error {
    /// A library needed for an interface lookup is not mapped in this process
    /// (`crate::module::Module::find_loaded` returned `None`).
    #[error("module {0:?} is not loaded in this process")]
    ModuleNotLoaded(String),

    /// `dlsym` did not resolve a symbol in the module.
    #[error("symbol {0:?} not found in the module")]
    SymbolMissing(String),

    /// A module was loaded but its `CreateInterface` factory returned null for the
    /// requested interface version.
    #[error("interface {0:?} not available in this process")]
    InterfaceMissing(String),

    /// The generated vtable binding has no typed signature for this slot (the entry is
    /// `*const c_void` in `s2sdk-sys`). Never `unwrap` a vtable entry — map it here.
    #[error("generated binding for slot {0} has no typed signature")]
    MissingSlot(&'static str),

    /// A call or data lookup returned null where the caller needed a value.
    #[error("call to {0} returned null")]
    NullReturn(&'static str),

    /// The requested object (class, event, convar, ...) does not exist.
    #[error("{0} not found")]
    NotFound(String),

    /// A C string is not valid UTF-8 (schema names, event payloads are expected to be
    /// ASCII/UTF-8; surfacing the mismatch beats `to_string_lossy` corruption).
    #[error("string is not valid UTF-8")]
    Utf8,

    /// `ConVar::value_string_on_game_thread` was called on a convar whose `m_eVarType` is
    /// not `EConVarType_String` (only string convars carry a string value).
    #[error("convar value type is {0:?}, not EConVarType_String")]
    CvarNotString(sys::EConVarType),
}

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;
