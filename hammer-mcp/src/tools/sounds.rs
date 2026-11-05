//! Sound tools (`hammer/snd-triggers`), appended after the trigger tools in [`SOUND_TOOL_ORDER`].
//! Soundevents (`.vsndevts`) define what a sound is; the sound entities (`tags( Sound )` FGD
//! classes: `snd_event_*`, `snd_soundscape`/`env_soundscape*`, `ambient_generic`, ...) play them at
//! points, in volumes and on I/O timing. `soundevents_query.rs`: `list_soundevents`;
//! `soundevents_edit.rs`: `edit_soundevents`, `compile_soundevents`; `sound_entities.rs`:
//! `create_sound_entities`; `sound_runtime.rs`: `drain_sounds`. Each module owns its specs; this
//! one fixes the order.
use super::{sound_entities, sound_runtime, soundevents_edit, soundevents_query};
use crate::state::HammerState;
use cs2_mcp_common::tools::Registry;

pub fn register(r: &mut Registry<HammerState>) {
    soundevents_query::register(r);
    soundevents_edit::register(r);
    sound_entities::register(r);
    sound_runtime::register(r);
}

/// The sound tools' order after [`super::triggers::TRIGGER_TOOL_ORDER`] (contract tests pin it).
pub const SOUND_TOOL_ORDER: [&str; 5] = [
    "list_soundevents",
    "edit_soundevents",
    "compile_soundevents",
    "create_sound_entities",
    "drain_sounds",
];
