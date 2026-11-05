//! Soundevents (`.vsndevts` / `.vsndevts_c`): the cached name index over the game VPKs and the
//! addon ([`index`]) and the comment-preserving text splice for the addon's
//! `soundevents_addon.vsndevts` ([`text`]). Plan: `target/orch/plan-snd.md`; RE facts:
//! `hammer-mcp/research/sounds-re.md` (written at the end of the pass).
pub mod index;
pub mod text;

pub use index::*;
