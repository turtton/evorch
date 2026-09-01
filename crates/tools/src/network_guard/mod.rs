//! main process で外向き HTTP 通信を防御する NetworkGuard。

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use reqwest::{
    Client, StatusCode, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{HeaderMap, LOCATION},
    redirect,
};

mod dns;
mod error;
mod https;
mod ip_policy;
mod size;

pub use dns::DnsResolver;
use dns::{HickoryResolver, PinningResolver};
pub use error::NetworkGuardError;
use https::upgrade_to_https;
use ip_policy::IpPolicy;

/// 追従を許可する redirect の最大回数。
pub const MAX_REDIRECTS: u32 = 10;
/// raw・転送・解凍後の各段階で許可する最大本文サイズ。
pub const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// NetworkGuard が検証済み本文として返す HTTP 応答。
#[derive(Debug)]
pub struct GuardedResponse {
    /// 最終応答の HTTP status。
    pub status: StatusCode,
    /// 最終応答の header。
    pub headers: HeaderMap,
    /// サイズ検証と必要な解凍を終えた本文。
    pub body: Vec<u8>,
}

/// HTTPS・DNS pinning・IP・redirect・本文サイズを一括強制する通信境界。
pub struct NetworkGuard {
    policy: IpPolicy,
    resolver: Arc<dyn DnsResolver>,
    root_certificate: Option<reqwest::Certificate>,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl NetworkGuard {
    /// system DNS resolver を使う production guard を構築する。
    ///
    /// # Errors
    /// OS の DNS 設定を読み込めない場合はエラーを返す。
    pub fn new() -> Result<Self, NetworkGuardError> {
        Ok(Self::with_resolver(Arc::new(HickoryResolver::new()?)))
    }

    /// 指定 resolver を使う guard を構築する。
    pub fn with_resolver(resolver: Arc<dyn DnsResolver>) -> Self {
        Self::with_resolver_root_certificate_and_timeouts(
            resolver,
            None,
            CONNECT_TIMEOUT,
            REQUEST_TIMEOUT,
        )
    }

    /// 指定 resolver と追加の信頼済み root 証明書を使う guard を構築する。
    ///
    /// loopback fixture 等、system trust store 外の HTTPS endpoint を検証する用途で使う。
    pub fn with_resolver_and_root_certificate(
        resolver: Arc<dyn DnsResolver>,
        root_certificate: reqwest::Certificate,
    ) -> Self {
        Self::with_resolver_root_certificate_and_timeouts(
            resolver,
            Some(root_certificate),
            CONNECT_TIMEOUT,
            REQUEST_TIMEOUT,
        )
    }

    /// resolver・root 証明書・接続/request timeout をすべて指定して構築する。
    ///
    /// 既定の timeout 定数では再現できない低速・停滞応答を扱う test 用の注入点で
    /// ある。request timeout で失敗した送信は種別が保持され、transport 層で
    /// search 層の `SearchError::Timeout`（fallback trigger）へ写像される。
    pub fn with_resolver_root_certificate_and_timeouts(
        resolver: Arc<dyn DnsResolver>,
        root_certificate: Option<reqwest::Certificate>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Self {
        Self {
            policy: IpPolicy,
            resolver,
            root_certificate,
            connect_timeout,
            request_timeout,
        }
    }

    /// URL を guard 下で取得する。
    ///
    /// 送信時の timeout は種別を保持して [`NetworkGuardError::Http`] として現れ、
    /// transport 層で search 層の `SearchError::Timeout`（fallback trigger、AC2）
    /// へ写像される。TLS 失敗などそれ以外の送信失敗は
    /// [`NetworkGuardError::HttpsConnectFailed`] になる。
    ///
    /// # Errors
    /// URL・DNS・IP・HTTPS・redirect・本文サイズのいずれかの検査または通信に失敗した場合、
    /// 対応する [`NetworkGuardError`] を返す。
    pub async fn get(&self, url: &str) -> Result<GuardedResponse, NetworkGuardError> {
        self.guarded_request(url, OutgoingRequest::Get).await
    }

    /// URL に JSON body を POST する。
    ///
    /// get() と同一の guard pipeline（HTTPS upgrade・DNS pinning・IP 検査・
    /// Content-Length / streaming / 解凍後のサイズ上限）を通過する。
    /// `headers` は既定の `Content-Type: application/json` に追加で merge され、
    /// POST 応答が 3xx を返した場合は Location の有無にかかわらず追従せず
    /// [`NetworkGuardError::RedirectOnPost`] で fail-closed になる。
    /// 送信時の timeout は [`NetworkGuardError::Http`] として種別が保持され、
    /// transport 層で search 層の `SearchError::Timeout`（fallback trigger、AC2）
    /// へ写像される。
    ///
    /// # Errors
    /// URL・DNS・IP・HTTPS・redirect・本文サイズのいずれかの検査または通信に失敗した場合、
    /// 対応する [`NetworkGuardError`] を返す。
    pub async fn post_json(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &serde_json::Value,
    ) -> Result<GuardedResponse, NetworkGuardError> {
        self.guarded_request(url, OutgoingRequest::PostJson { headers, body })
            .await
    }

    async fn guarded_request(
        &self,
        url: &str,
        request: OutgoingRequest<'_>,
    ) -> Result<GuardedResponse, NetworkGuardError> {
        let pinning = Arc::new(PinningResolver::new(self.resolver.clone()));
        let adapter = Arc::new(ReqwestPinningResolver {
            resolver: pinning.clone(),
        });
        let mut builder = Client::builder()
            .redirect(redirect::Policy::none())
            .dns_resolver(adapter)
            .no_proxy()
            .danger_accept_invalid_certs(false)
            .connect_timeout(self.connect_timeout)
            .timeout(self.request_timeout);
        if let Some(certificate) = self.root_certificate.clone() {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build()?;
        let mut current = upgrade_to_https(url)?;
        let mut redirects = 0_u32;

        loop {
            ensure_https(&current)?;
            let host = current.host_str().ok_or(NetworkGuardError::MissingHost)?;
            let addrs = pinning.resolve(host).await?;
            for addr in addrs {
                if self.policy.is_blocked(addr) {
                    return Err(NetworkGuardError::BlockedIp { addr });
                }
            }

            let built = match request.clone() {
                OutgoingRequest::Get => client.get(current.clone()),
                OutgoingRequest::PostJson { headers, body } => {
                    client.post(current.clone()).json(body).headers(headers)
                }
            };
            let mut response = built.send().await.map_err(map_send_error)?;
            if intercepts_redirect(&request, response.status()) {
                match &request {
                    OutgoingRequest::PostJson { .. } => {
                        let location = response
                            .headers()
                            .get(LOCATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        return Err(NetworkGuardError::RedirectOnPost { location });
                    }
                    OutgoingRequest::Get => {
                        if redirects == MAX_REDIRECTS {
                            return Err(NetworkGuardError::TooManyRedirects);
                        }
                        current = redirect_target(&current, response.headers())?;
                        redirects += 1;
                        continue;
                    }
                }
            }

            let status = response.status();
            let headers = response.headers().clone();
            size::check_content_length(&headers)?;
            let mut raw = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                size::append_chunk(&mut raw, &chunk)?;
            }
            let body = size::decode(&headers, &raw)?;
            return Ok(GuardedResponse {
                status,
                headers,
                body,
            });
        }
    }
}

/// guard が送信する request の形状。
#[derive(Clone)]
enum OutgoingRequest<'a> {
    Get,
    PostJson {
        headers: HeaderMap,
        body: &'a serde_json::Value,
    },
}

/// request 送信時の失敗を種別を保持したまま写像する。
///
/// timeout は [`NetworkGuardError::Http`] として保持し、MCP transport 層で
/// search 層の `SearchError::Timeout`（fallback trigger）へ写像される。それ以外
/// （TLS handshake 失敗・接続拒否など）は従来どおり
/// [`NetworkGuardError::HttpsConnectFailed`] に畳む。
fn map_send_error(error: reqwest::Error) -> NetworkGuardError {
    if error.is_timeout() {
        NetworkGuardError::Http(error)
    } else {
        NetworkGuardError::HttpsConnectFailed(error)
    }
}

/// request 形状ごとに、guard が横取りする redirect status を判定する。
///
/// GET は従来どおり追従対象の 3xx を追従し、POST は Location の有無に
/// かかわらずすべての 3xx を fail-closed で拒否する。
fn intercepts_redirect(request: &OutgoingRequest<'_>, status: StatusCode) -> bool {
    match request {
        OutgoingRequest::Get => is_redirect(status),
        OutgoingRequest::PostJson { .. } => status.is_redirection(),
    }
}

fn ensure_https(url: &Url) -> Result<(), NetworkGuardError> {
    match url.scheme() {
        "https" => Ok(()),
        "http" => Err(NetworkGuardError::NotHttpsAfterUpgrade),
        scheme => Err(NetworkGuardError::NotHttpScheme {
            scheme: scheme.to_owned(),
        }),
    }
}

const fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn redirect_target(current: &Url, headers: &HeaderMap) -> Result<Url, NetworkGuardError> {
    let location = headers
        .get(LOCATION)
        .ok_or_else(|| {
            NetworkGuardError::RedirectLocationInvalid("Location がありません".to_owned())
        })?
        .to_str()
        .map_err(|error| NetworkGuardError::RedirectLocationInvalid(error.to_string()))?;
    let target = current
        .join(location)
        .map_err(|error| NetworkGuardError::RedirectLocationInvalid(error.to_string()))?;
    upgrade_to_https(target.as_str())
}

struct ReqwestPinningResolver {
    resolver: Arc<PinningResolver>,
}

impl Resolve for ReqwestPinningResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.resolver.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = resolver
                .resolve(&host)
                .await
                .map_err(|error| io::Error::other(error.to_string()))?;
            let socket_addrs: Vec<SocketAddr> = addrs
                .into_iter()
                .map(|addr| SocketAddr::new(addr, 0))
                .collect();
            let addrs: Addrs = Box::new(socket_addrs.into_iter());
            Ok(addrs)
        })
    }
}
