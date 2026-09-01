use std::{collections::HashMap, net::IpAddr, sync::Arc};

use async_trait::async_trait;
use hickory_resolver::TokioResolver;
use tokio::sync::Mutex;

use super::NetworkGuardError;

/// host 名を接続先 IP 集合へ解決する境界。
#[async_trait]
pub trait DnsResolver: Send + Sync {
    /// host を解決する。
    ///
    /// # Errors
    /// DNS 問い合わせまたは host の解析に失敗した場合はエラーを返す。
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetworkGuardError>;
}

pub(crate) struct HickoryResolver {
    resolver: TokioResolver,
}

impl HickoryResolver {
    pub(crate) fn new() -> Result<Self, NetworkGuardError> {
        TokioResolver::builder_tokio()
            .map(|builder| Self {
                resolver: builder.build(),
            })
            .map_err(|error| NetworkGuardError::DnsResolverInitialization(error.to_string()))
    }
}

#[async_trait]
impl DnsResolver for HickoryResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        if let Ok(addr) = host.parse() {
            return Ok(vec![addr]);
        }
        if host.eq_ignore_ascii_case("localhost") {
            return Ok(vec![
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ]);
        }

        let addrs: Vec<IpAddr> = self
            .resolver
            .lookup_ip(host)
            .await
            .map_err(|error| NetworkGuardError::DnsResolutionFailed {
                host: host.to_owned(),
                detail: error.to_string(),
            })?
            .iter()
            .collect();
        if addrs.is_empty() {
            return Err(NetworkGuardError::DnsResolutionFailed {
                host: host.to_owned(),
                detail: "解決結果が空です".to_owned(),
            });
        }
        Ok(addrs)
    }
}

pub(crate) struct PinningResolver {
    base: Arc<dyn DnsResolver>,
    cache: Mutex<HashMap<String, Vec<IpAddr>>>,
}

impl PinningResolver {
    pub(crate) fn new(base: Arc<dyn DnsResolver>) -> Self {
        Self {
            base,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl DnsResolver for PinningResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        if let Ok(addr) = host.parse() {
            return Ok(vec![addr]);
        }
        if host.eq_ignore_ascii_case("localhost") {
            return Ok(vec![
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ]);
        }
        let mut cache = self.cache.lock().await;
        if let Some(addrs) = cache.get(host) {
            return Ok(addrs.clone());
        }
        let addrs = self.base.resolve(host).await?;
        cache.insert(host.to_owned(), addrs.clone());
        Ok(addrs)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct ChangingResolver {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl DnsResolver for ChangingResolver {
        async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let addr = if call == 0 {
                Ipv4Addr::new(93, 184, 216, 34)
            } else {
                Ipv4Addr::new(169, 254, 169, 254)
            };
            Ok(vec![IpAddr::V4(addr)])
        }
    }

    // Given: 2 回目に遮断 IP を返す resolver / When: 同一 host を2回解決 / Then: 初回結果だけが固定される
    #[tokio::test]
    async fn pins_first_answer_for_request_lifetime() {
        let base = Arc::new(ChangingResolver {
            calls: AtomicUsize::new(0),
        });
        let resolver = PinningResolver::new(base.clone());

        let first = resolver
            .resolve("example.test")
            .await
            .expect("初回解決は成功する");
        let second = resolver
            .resolve("example.test")
            .await
            .expect("固定済み解決は成功する");

        assert_eq!(first, vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]);
        assert_eq!(second, first);
        assert_eq!(base.calls.load(Ordering::SeqCst), 1);
    }

    // Given: IP literal と localhost / When: Hickory を使わず短絡解決 / Then: offline-safe な固定 IP が返る
    #[tokio::test]
    async fn resolves_literals_and_localhost_without_dns() {
        let resolver = HickoryResolver::new().expect("system resolver を構築できる");

        assert_eq!(
            resolver
                .resolve("127.0.0.2")
                .await
                .expect("literal は解決できる"),
            vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))]
        );
        assert_eq!(
            resolver
                .resolve("LOCALHOST")
                .await
                .expect("localhost は解決できる"),
            vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            ]
        );
    }
}
