// intentional: world/world.rs is the domain core.
#[allow(clippy::module_inception)]
pub mod world;
pub mod inputs;
pub mod history;
pub mod model_bridge;
pub mod event_log;
pub mod effects;
pub mod gates;
pub mod budget;
pub mod edge;
pub mod edges;
pub mod autonomy;
pub mod lifecycle;
pub mod surface;
pub mod mode;
pub mod systems;
pub mod replay;
pub mod snapshot;
pub mod blob;
pub mod branch;
pub mod retention;
pub mod segment;
pub mod reclaim;
pub mod driver;
