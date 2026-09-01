mod common;

use std::{
    io::Write,
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use flate2::{
    Compression,
    write::{DeflateEncoder, GzEncoder},
};
use tools::{DnsResolver, MAX_RESPONSE_BYTES, NetworkGuard, NetworkGuardError};

use common::{
    FixtureServer, TestResult, chunked_response, identity_response, redirect, response_with_headers,
};

struct CountingResolver {
    addr: IpAddr,
    calls: AtomicUsize,
}

#[async_trait]
impl DnsResolver for CountingResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.addr])
    }
}

fn guard(server: &FixtureServer) -> (NetworkGuard, Arc<CountingResolver>) {
    let resolver = Arc::new(CountingResolver {
        addr: server.resolver_addr(),
        calls: AtomicUsize::new(0),
    });
    let guard =
        NetworkGuard::with_resolver_and_root_certificate(resolver.clone(), server.certificate());
    (guard, resolver)
}

// Given: plain HTTP fixture の port を https URL として指定 / When: guard で取得 / Then: TLS 失敗を返して HTTP fallback しない
#[tokio::test]
async fn refuses_http_fallback_after_https_failure() -> TestResult {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let served = Arc::new(AtomicUsize::new(0));
    let observed = served.clone();
    tokio::spawn(async move {
        if let Ok((mut stream, _peer)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut bytes = [0_u8; 16];
            if let Ok(read) = stream.read(&mut bytes).await
                && bytes[..read].starts_with(b"GET ")
            {
                observed.fetch_add(1, Ordering::SeqCst);
                let _write_result = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                    .await;
            }
        }
    });
    let resolver = Arc::new(CountingResolver {
        addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        calls: AtomicUsize::new(0),
    });
    let guard = NetworkGuard::with_resolver(resolver);

    let error = guard
        .get(&format!("https://fixture.test:{}/", addr.port()))
        .await
        .expect_err("plain HTTP fixture への TLS 接続は失敗する");

    assert!(matches!(error, NetworkGuardError::HttpsConnectFailed(_)));
    assert_eq!(served.load(Ordering::SeqCst), 0);
    Ok(())
}

// Given: 10 redirects の HTTPS chain / When: guard で取得 / Then: final 200 と本文を返す
#[tokio::test]
async fn follows_exactly_ten_redirects() -> TestResult {
    let server = FixtureServer::start(|path| {
        if path == "/final" {
            return identity_response(b"OK");
        }
        let index = path.trim_start_matches("/r").parse::<u32>().unwrap_or(0);
        if index == 9 {
            redirect("/final")
        } else {
            redirect(&format!("/r{}", index + 1))
        }
    })
    .await?;
    let (guard, _resolver) = guard(&server);

    let response = guard.get(&server.url("/r0")).await?;

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"OK");
    Ok(())
}

// Given: 11 redirects の HTTPS chain / When: guard で取得 / Then: redirect 上限エラーを返す
#[tokio::test]
async fn rejects_eleventh_redirect() -> TestResult {
    let server = FixtureServer::start(|path| {
        let index = path.trim_start_matches("/r").parse::<u32>().unwrap_or(0);
        redirect(&format!("/r{}", index + 1))
    })
    .await?;
    let (guard, _resolver) = guard(&server);

    let error = guard
        .get(&server.url("/r0"))
        .await
        .expect_err("11回目は拒否される");

    assert!(matches!(error, NetworkGuardError::TooManyRedirects));
    Ok(())
}

// Given: link-local へ飛ぶ redirect / When: guard で追従 / Then: 接続前に BlockedIp を返す
#[tokio::test]
async fn reguards_redirect_target_before_connection() -> TestResult {
    let server =
        FixtureServer::start(|_path| redirect("https://169.254.169.254/latest/meta-data")).await?;
    let (guard, _resolver) = guard(&server);

    let error = guard
        .get(&server.url("/start"))
        .await
        .expect_err("link-local は拒否される");

    assert!(
        matches!(error, NetworkGuardError::BlockedIp { addr } if addr == IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)))
    );
    Ok(())
}

// Given: 同一 host の redirect chain / When: guard で取得 / Then: base DNS resolver は一度だけ呼ばれる
#[tokio::test]
async fn pins_dns_across_redirect_chain() -> TestResult {
    let server = FixtureServer::start(|path| {
        if path == "/start" {
            redirect("/final")
        } else {
            identity_response(b"OK")
        }
    })
    .await?;
    let (guard, resolver) = guard(&server);

    let response = guard.get(&server.url("/start")).await?;

    assert_eq!(response.body, b"OK");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

// Given: 6MB と宣言して本文をほぼ送らない応答 / When: guard で取得 / Then: body 読み取り前の size error を返す
#[tokio::test]
async fn rejects_oversized_content_length_before_body_read() -> TestResult {
    let server = FixtureServer::start(|_path| {
        response_with_headers(&[format!("Content-Length: {}", 6 * 1024 * 1024)], b"x")
    })
    .await?;
    let (guard, _resolver) = guard(&server);

    let error = guard
        .get(&server.url("/"))
        .await
        .expect_err("Content-Length 超過は拒否される");

    assert!(matches!(
        error,
        NetworkGuardError::ResponseTooLarge {
            check: "Content-Length",
            ..
        }
    ));
    Ok(())
}

// Given: Content-Length なしで 5MB 超を chunked 送信 / When: guard で取得 / Then: streaming size error を返す
#[tokio::test]
async fn rejects_oversized_streamed_body() -> TestResult {
    let server = FixtureServer::start(|_path| {
        chunked_response(std::iter::repeat_with(|| vec![0_u8; 1024 * 1024]).take(6))
    })
    .await?;
    let (guard, _resolver) = guard(&server);

    let error = guard
        .get(&server.url("/"))
        .await
        .expect_err("streaming 超過は拒否される");

    assert!(matches!(
        error,
        NetworkGuardError::ResponseTooLarge {
            check: "streaming",
            ..
        }
    ));
    Ok(())
}

// Given: 解凍後に 5MB を超える小さい gzip / When: guard で取得 / Then: decompressed size error を返す
#[tokio::test]
async fn rejects_oversized_decompressed_gzip() -> TestResult {
    let compressed = gzip(&vec![0_u8; MAX_RESPONSE_BYTES + 1])?;
    let server = FixtureServer::start(move |_path| {
        response_with_headers(
            &[
                "Content-Encoding: gzip".to_owned(),
                format!("Content-Length: {}", compressed.len()),
            ],
            &compressed,
        )
    })
    .await?;
    let (guard, _resolver) = guard(&server);

    let error = guard
        .get(&server.url("/"))
        .await
        .expect_err("解凍後超過は拒否される");

    assert!(matches!(
        error,
        NetworkGuardError::ResponseTooLarge {
            check: "decompressed",
            ..
        }
    ));
    Ok(())
}

// Given: 小さい gzip・deflate・identity 応答 / When: guard で取得 / Then: 元本文へ復元される
#[tokio::test]
async fn decodes_supported_bodies_within_limit() -> TestResult {
    let gzip_body = gzip(b"gzip-ok")?;
    let deflate_body = deflate(b"deflate-ok")?;
    let server = FixtureServer::start(move |path| match path {
        "/gzip" => response_with_headers(
            &[
                "Content-Encoding: gzip".to_owned(),
                format!("Content-Length: {}", gzip_body.len()),
            ],
            &gzip_body,
        ),
        "/deflate" => response_with_headers(
            &[
                "Content-Encoding: deflate".to_owned(),
                format!("Content-Length: {}", deflate_body.len()),
            ],
            &deflate_body,
        ),
        _ => identity_response(b"identity-ok"),
    })
    .await?;
    let (guard, _resolver) = guard(&server);

    let gzip_response = guard.get(&server.url("/gzip")).await?;
    let deflate_response = guard.get(&server.url("/deflate")).await?;
    let identity = guard.get(&server.url("/identity")).await?;

    assert_eq!(gzip_response.body, b"gzip-ok");
    assert_eq!(deflate_response.body, b"deflate-ok");
    assert_eq!(identity.body, b"identity-ok");
    Ok(())
}

fn gzip(body: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body)?;
    encoder.finish()
}

fn deflate(body: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body)?;
    encoder.finish()
}
