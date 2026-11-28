//! Injectable fetch function for end-to-end tests; the production default is
//! always the native account-scoped client. Scope host construction, not a query.
use super::*;
/// Like the settings model-fetch seam: an async function, no real endpoints.
pub type QuotaFetch = Arc<
    dyn Fn(String, QuotaProvider) -> BoxFuture<'static, Result<Vec<QuotaWindow>, QuotaIssue>>
        + Send
        + Sync,
>;
tokio::task_local! { static FETCH: QuotaFetch; }
/// Construct a host with a quota fetch substitute. It is captured by that host
/// and does not affect other hosts/tests running on the same thread.
pub async fn with_fetch<F: std::future::Future>(fetch: QuotaFetch, work: F) -> F::Output {
    FETCH.scope(fetch, work).await
}
pub(super) fn injected() -> Option<Fetch> {
    let fetch = FETCH.try_with(Clone::clone).ok()?;
    Some(Arc::new(move |profile| {
        Ok(Box::new(Function {
            name: profile.name.clone(),
            kind: profile.kind,
            fetch: fetch.clone(),
        }))
    }))
}
struct Function {
    name: String,
    kind: QuotaProvider,
    fetch: QuotaFetch,
}
#[async_trait::async_trait]
impl Backend for Function {
    async fn fetch(&mut self) -> Result<Vec<QuotaWindow>, QuotaIssue> {
        (self.fetch)(self.name.clone(), self.kind).await
    }
}
