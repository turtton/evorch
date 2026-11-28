//! Metadata only: no executor, browser, network, process, file or lease is opened.
//! [`standard_tools`] lists the standard tools only; the host/runtime tools are not in this
//! module, and the optional browser, web and LSP tools are in [`optional_tools`].
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
struct MetadataOnlyResolver;
#[async_trait::async_trait]
impl crate::DnsResolver for MetadataOnlyResolver {
    async fn resolve(&self, _: &str) -> Result<Vec<std::net::IpAddr>, crate::NetworkGuardError> {
        unreachable!("metadata-only network tools never resolve a host")
    }
}

struct MetadataOnlyPages;
#[async_trait::async_trait]
impl browser::agent::PageProvider for MetadataOnlyPages {
    async fn page(
        &self,
        _: &str,
    ) -> Result<Arc<browser::agent::PageAgent>, browser::agent::AgentError> {
        Err(browser::agent::AgentError::Refused(
            "metadata-only browser tools never open a page".into(),
        ))
    }
    async fn open_page(&self, _: &str) -> Option<Arc<browser::agent::PageAgent>> {
        None
    }
    async fn close(&self, _: &str) -> bool {
        false
    }
    fn release(&self, _: &str) {}
}

/// Metadata of the tools that are registered only when configured: the agent browser
/// tools (with the coordinate action), `web_search`, `web_fetch` and `lsp_diagnostics`.
/// Like [`standard_tools`] it opens no browser, network, process or file; it exists so
/// that a check can cover every model-facing schema, not so that these tools register.
pub fn optional_tools() -> Vec<crate::executor::ToolSpec> {
    let guard = Arc::new(crate::NetworkGuard::with_resolver(Arc::new(
        MetadataOnlyResolver,
    )));
    let mut tools: Vec<Arc<dyn Tool>> = crate::browser_tools(
        Arc::new(MetadataOnlyPages),
        crate::BrowserToolOptions {
            coordinate_actions: true,
        },
    )
    .into_iter()
    .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
    .collect();
    tools.push(Arc::new(crate::WebFetch::with_guard(Arc::clone(&guard))));
    tools.push(Arc::new(crate::WebSearch::for_providers(
        Arc::new(crate::ExaKeylessProvider::with_guard(Arc::clone(&guard))),
        Arc::new(crate::ExaKeylessProvider::with_guard(guard)),
    )));
    tools.push(Arc::new(crate::lsp::LspDiagnostics::new(
        Arc::new(MetadataOnly),
        sandbox::CommandSpec {
            program: String::new(),
            args: Vec::new(),
            cwd: None,
            extra_env: Vec::new(),
        },
        String::new(),
    )));
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
    fn optional_catalog_lists_browser_web_and_lsp_tools_by_name() {
        let names: Vec<_> = super::optional_tools()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        let mut expected: Vec<String> = crate::BROWSER_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .chain(["web_fetch", "web_search", "lsp_diagnostics"].map(String::from))
            .collect();
        expected.sort();
        assert_eq!(names, expected);
    }

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
