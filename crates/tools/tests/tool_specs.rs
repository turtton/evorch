use std::sync::Arc;

use event_bus::EventBus;
use sandbox::DirectSandbox;
use tools::{Edit, GitDiff, Grep, Read, Shell, Tool, ToolExecutor, WebFetch, WebSearch};

#[test]
fn tool_specs_preserve_registered_schemas_and_descriptions() {
    // Given: all seven standard tools registered through the public API.
    let sandbox = Arc::new(DirectSandbox::new_unchecked());
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(Read),
        Arc::new(Edit),
        Arc::new(Grep),
        Arc::new(Shell::new(sandbox.clone())),
        Arc::new(GitDiff::new(sandbox)),
        Arc::new(WebSearch::keyless_default().unwrap()),
        Arc::new(WebFetch::new().unwrap()),
    ];
    let mut executor = ToolExecutor::new(Arc::new(EventBus::new(16)));
    for tool in &tools {
        executor.register(Arc::clone(tool)).unwrap();
    }

    // When: the model-facing definitions are requested.
    let specs = executor.tool_specs();

    // Then: every registered schema is preserved, including read's parameters.
    assert_eq!(specs.len(), tools.len());
    for tool in &tools {
        let spec = specs.iter().find(|spec| spec.name == tool.name()).unwrap();
        assert_eq!(spec.input_schema, tool.schema());
        assert!(!spec.description.trim().is_empty());
        assert_ne!(spec.description, format!("{} tool", tool.name()));
    }
    let read = specs.iter().find(|spec| spec.name == "read").unwrap();
    assert!(
        !read.input_schema["properties"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}
