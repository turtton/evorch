use reqwest::Url;

use super::NetworkGuardError;

pub(crate) fn upgrade_to_https(url: &str) -> Result<Url, NetworkGuardError> {
    let mut parsed =
        Url::parse(url).map_err(|error| NetworkGuardError::InvalidUrl(error.to_string()))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" => {
            parsed
                .set_scheme("https")
                .map_err(|()| NetworkGuardError::NotHttpsAfterUpgrade)?;
            Ok(parsed)
        }
        scheme => Err(NetworkGuardError::NotHttpScheme {
            scheme: scheme.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: http URL / When: HTTPS へ upgrade / Then: host・port・path・query を保った https URL になる
    #[test]
    fn upgrades_http_while_preserving_url_parts() {
        let upgraded = upgrade_to_https("http://example.test:8443/path?q=1")
            .expect("有効な http URL は upgrade できる");

        assert_eq!(upgraded.as_str(), "https://example.test:8443/path?q=1");
    }

    // Given: https URL / When: HTTPS へ upgrade / Then: URL は変更されない
    #[test]
    fn preserves_https_url() {
        let upgraded =
            upgrade_to_https("https://example.test/path").expect("有効な https URL は受理される");

        assert_eq!(upgraded.as_str(), "https://example.test/path");
    }

    // Given: ws URL / When: HTTPS へ upgrade / Then: 非 HTTP scheme として拒否される
    #[test]
    fn rejects_non_http_scheme() {
        let error = upgrade_to_https("ws://example.test/socket").expect_err("ws URL は拒否される");

        assert!(matches!(error, NetworkGuardError::NotHttpScheme { .. }));
    }
}
