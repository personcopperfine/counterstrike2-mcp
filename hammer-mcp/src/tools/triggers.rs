//! Trigger tools (`hammer/triggers`), appended after the prop tools in [`TRIGGER_TOOL_ORDER`].
//! A trigger is a brush entity: a `CMapEntity` whose FGD class is a `@SolidClass` with tied
//! `CMapMesh` children (the volume, normally `materials/tools/toolstrigger.vmat`); it fires outputs
//! (OnStartTouch, OnTrigger, ...) into other entities' inputs through the entity I/O tools.
//! `triggers_query.rs`: `list_triggers` (SAVED `.vmap` + live document);
//! `triggers_live.rs`: `create_trigger`, `tie_to_entity`, `move_to_world` (bridge m2.19
//! `entity.tieMeshes`/`entity.untie`); `trigger_volume.rs`: `set_trigger_volume`.
//! Each module owns its specs; this one fixes the order.
use super::{trigger_volume, triggers_live, triggers_query};
use crate::state::HammerState;
use cs2_mcp_common::tools::Registry;

pub fn register(r: &mut Registry<HammerState>) {
    triggers_query::register(r);
    triggers_live::register(r);
    trigger_volume::register(r);
}

/// The trigger tools' order after [`super::props::PROP_TOOL_ORDER`] (contract tests pin it).
pub const TRIGGER_TOOL_ORDER: [&str; 5] = [
    "list_triggers",
    "create_trigger",
    "tie_to_entity",
    "move_to_world",
    "set_trigger_volume",
];
