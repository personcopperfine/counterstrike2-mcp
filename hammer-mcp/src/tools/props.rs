//! Prop tools (`hammer/props`), appended after the particle tools in [`PROP_TOOL_ORDER`].
//! A prop is a `CMapEntity` whose FGD class is a model entity: `prop_static` is compile-time
//! (resourcecompiler bakes it into the world), `prop_dynamic*`/`prop_physics*` are runtime entities.
//! `model_catalog.rs`: the model list (game VPKs + the addon) and model existence checks;
//! `model_info.rs`: compiled `.vmdl_c` facts (bounds, skins, bodygroups, sequences);
//! `props_query.rs`: prop reads over the SAVED `.vmap` and the live document;
//! `props_live.rs`: `create_props`; `node_transform.rs`: `set_node_transform` (bridge m2.18
//! `node.setTransform`); `entity_io.rs`: entity I/O connections (m2.18 `entity.add/removeConnections`).
//! Each module owns its specs; this one fixes the order.
use super::{assets, entity_io, model_catalog, model_info, node_transform, props_live, props_query};
use crate::state::HammerState;
use cs2_mcp_common::tools::Registry;

pub fn register(r: &mut Registry<HammerState>) {
    model_catalog::register(r);
    assets::register(r);
    model_info::register(r);
    props_query::register(r);
    props_live::register(r);
    node_transform::register(r);
    entity_io::register(r);
}

/// The prop tools' order after [`super::particles::PET_TOOL_ORDER`] (contract tests pin it).
pub const PROP_TOOL_ORDER: [&str; 8] = [
    "list_models",
    "find_assets",
    "get_model_info",
    "list_props",
    "create_props",
    "set_node_transform",
    "add_entity_connections",
    "remove_entity_connections",
];
