//! Metadata only: no executor, browser, network, process, file or lease is opened.
//! Host/runtime tools and optional network/browser tools are not in this catalog.
use crate::{ApplyPatch, Edit, GitDiff, Grep, Read, RunJs, Shell, Tool, Write};
use std::sync::Arc;

struct MetadataOnly;
impl sandbox::Sandbox for MetadataOnly {
    fn wrap(
        &self,
        _: sandbox::CommandSpec,
    ) -> Result<sandbox::WrappedCommand, sandbox::SandboxError> {
        unreachable!("metadata-only tool instances are never executable")
    }
}
/// The same name-sorted order as ToolExecutor::tool_specs. Actual Tool trait
/// metadata is read without registering tools or configuring any sandbox.
pub fn standard_tools() -> Vec<crate::executor::ToolSpec> {
    let sandbox = Arc::new(MetadataOnly);
    let tools: [Arc<dyn Tool>; 8] = [
        Arc::new(Read),
        Arc::new(Edit::default()),
        Arc::new(Write::default()),
        Arc::new(ApplyPatch::new()),
        Arc::new(Grep::default()),
        Arc::new(Shell::new(sandbox.clone())),
        Arc::new(GitDiff::new(sandbox)),
        Arc::new(RunJs::new()),
    ];
    let mut specs = tools
        .iter()
        .map(|tool| crate::executor::ToolSpec {
            name: tool.name().into(),
            description: tool.description().into(),
            input_schema: tool.schema(),
        })
        .collect::<Vec<_>>();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}
#[cfg(test)]
mod tests {
    #[test]
    fn compiled_standard_catalog_matches_actual_registration_name_schema_and_order() {
        let actual = crate::ToolExecutor::with_standard_tools(
            std::sync::Arc::new(event_bus::EventBus::new(4)),
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )
        .tool_specs();
        assert_eq!(super::standard_tools(), actual);
    }
}
