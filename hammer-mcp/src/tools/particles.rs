//! Particle editor tools (`.vpcf` particle systems), appended after the ModelDoc tools in
//! [`PET_TOOL_ORDER`]: the file route (`particle_file.rs`: read/list/validate/edit/create/compile)
//! then the running pet through the bridge (`particle_live.rs`: status/types/schema/load/open/
//! reload/save; `particle_live_edit.rs`: live read/edit/undo/redo; `particle_render.rs`: the
//! preview render). Each module owns its specs; this one fixes the order. RE: research/pet-re.md.
use super::{particle_file, particle_live, particle_live_edit, particle_render};
use crate::state::HammerState;
use cs2_mcp_common::tools::Registry;

pub fn register(r: &mut Registry<HammerState>) {
    particle_file::register(r);
    particle_live::register(r);
    particle_live_edit::register(r);
    particle_render::register(r);
}

/// The particle tools' order after [`super::modeldoc::MODELDOC_TOOL_ORDER`] (contract tests pin it).
pub const PET_TOOL_ORDER: [&str; 18] = [
    "particle_read",
    "particle_list",
    "particle_validate",
    "particle_edit",
    "particle_create",
    "particle_compile",
    "particle_status",
    "particle_types",
    "particle_schema",
    "particle_load",
    "particle_open",
    "particle_reload",
    "particle_save",
    "particle_live_read",
    "particle_live_edit",
    "particle_undo",
    "particle_redo",
    "particle_render",
];
