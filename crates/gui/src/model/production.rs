// Keep the old GUI API while the owner composition moves to workbench-host.
pub use workbench_host::bootstrap::models::{
    ProductionModel, ProjectModels, compose_production_model, reload_models,
};

#[cfg(test)]
mod tests;
