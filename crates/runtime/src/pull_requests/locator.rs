//! Reading a pull request reference a person typed or pasted: a github.com URL or
//! `owner/repo#number`.
//!
//! The host is checked strictly. Only the exact host `github.com` is accepted: not
//! Enterprise, not `www.`/`api.`/`gist.` subdomains, not a lookalike
//! (`github.com.evil.com`, `github.com@evil.com`), not a host with a port, user info, a
//! trailing dot or non-ASCII characters, and only over `https`.

use storage::pull_requests::PullRequestKey;

use super::FetchError;

/// Why a reference was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocatorError {
    /// The URL is not on exactly github.com over https.
    #[error("only https://github.com pull requests are supported ({0})")]
    InvalidHost(&'static str),
    /// The text is not a pull request reference.
    #[error("not a pull request reference: {0}")]
    Malformed(&'static str),
}

impl From<LocatorError> for FetchError {
    fn from(error: LocatorError) -> Self {
        match error {
            LocatorError::InvalidHost(reason) => Self::InvalidHost((*reason).to_owned()),
            LocatorError::Malformed(detail) => Self::InvalidReference(detail.to_owned()),
        }
    }
}

/// Parses `https://github.com/owner/repo/pull/123` (optionally followed by `/files`,
/// `/commits` or `/checks`, a query or a fragment) or `owner/repo#123` into a key.
///
/// # Errors
/// [`LocatorError::InvalidHost`] for a URL that is not exactly `https://github.com`;
/// [`LocatorError::Malformed`] for anything else that is not a pull request reference.
pub fn parse_locator(text: &str) -> Result<PullRequestKey, LocatorError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(LocatorError::Malformed("empty"));
    }
    if !text.is_ascii()
        || text
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(LocatorError::Malformed(
            "whitespace, control, backslash and non-ASCII characters are not allowed",
        ));
    }
    if let Some((scheme, rest)) = text.split_once("://") {
        return parse_url(scheme, rest);
    }
    parse_short(text)
}

fn parse_url(scheme: &str, rest: &str) -> Result<PullRequestKey, LocatorError> {
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(LocatorError::InvalidHost("the scheme must be https"));
    }
    if !authority.eq_ignore_ascii_case("github.com") {
        return Err(LocatorError::InvalidHost(
            "the host must be exactly github.com",
        ));
    }
    let path = tail
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    let (owner, name, number) = match segments.as_slice() {
        ["", owner, name, "pull", number]
        | [
            "",
            owner,
            name,
            "pull",
            number,
            "files" | "commits" | "checks",
        ] => (*owner, *name, *number),
        _ => return Err(LocatorError::Malformed("expected /owner/repo/pull/number")),
    };
    key(owner, name, number)
}

fn parse_short(text: &str) -> Result<PullRequestKey, LocatorError> {
    let (repository, number) = text
        .split_once('#')
        .ok_or(LocatorError::Malformed("expected owner/repo#number"))?;
    let (owner, name) = repository
        .split_once('/')
        .ok_or(LocatorError::Malformed("expected owner/repo#number"))?;
    key(owner, name, number)
}

fn key(owner: &str, name: &str, number: &str) -> Result<PullRequestKey, LocatorError> {
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(LocatorError::Malformed("the number must be digits"));
    }
    let number: u64 = number
        .parse()
        .map_err(|_| LocatorError::Malformed("the number is out of range"))?;
    PullRequestKey::github(&format!("{owner}/{name}"), number)
        .map_err(|_| LocatorError::Malformed("not a GitHub owner/name or number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(text: &str) -> PullRequestKey {
        parse_locator(text).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    // Given: the forms of one pull request / When: parsed / Then: the same key
    #[test]
    fn urls_and_short_references_name_the_same_pull_request() {
        let expected = PullRequestKey::github("turtton/evorch", 481).unwrap();
        for text in [
            "https://github.com/turtton/evorch/pull/481",
            "https://github.com/turtton/evorch/pull/481/",
            "HTTPS://GitHub.COM/Turtton/EvOrch/pull/481",
            "https://github.com/turtton/evorch/pull/481/files",
            "https://github.com/turtton/evorch/pull/481/commits",
            "https://github.com/turtton/evorch/pull/481/checks?check_run_id=5",
            "https://github.com/turtton/evorch/pull/481#issuecomment-1",
            "https://github.com/turtton/evorch/pull/481?diff=split",
            "turtton/evorch#481",
            "  Turtton/EvOrch#481 ",
        ] {
            assert_eq!(key_of(text), expected, "{text}");
        }
    }

    // Given: hosts that are not exactly github.com / When: parsed / Then: InvalidHost
    #[test]
    fn only_https_github_com_is_accepted() {
        for text in [
            "http://github.com/o/r/pull/1",
            "https://www.github.com/o/r/pull/1",
            "https://api.github.com/o/r/pull/1",
            "https://gist.github.com/o/r/pull/1",
            "https://github.example.com/o/r/pull/1",
            "https://github.com.evil.com/o/r/pull/1",
            "https://evilgithub.com/o/r/pull/1",
            "https://github.com:443/o/r/pull/1",
            "https://github.com:8443/o/r/pull/1",
            "https://user@github.com/o/r/pull/1",
            "https://user:pass@github.com/o/r/pull/1",
            "https://github.com@evil.com/o/r/pull/1",
            "https://evil.com@github.com/o/r/pull/1",
            "https://github.com./o/r/pull/1",
            "https://ghe.corp.example/o/r/pull/1",
            "ssh://git@github.com/o/r/pull/1",
            "ftp://github.com/o/r/pull/1",
        ] {
            assert!(
                matches!(parse_locator(text), Err(LocatorError::InvalidHost(_))),
                "{text}: {:?}",
                parse_locator(text)
            );
        }
        assert!(matches!(
            FetchError::from(parse_locator("https://ghe.corp.example/o/r/pull/1").unwrap_err()),
            FetchError::InvalidHost(_)
        ));
    }

    // Given: malformed references / When: parsed / Then: Malformed, never a guess
    #[test]
    fn malformed_references_are_refused() {
        for text in [
            "",
            "   ",
            "o/r",
            "o/r#",
            "o/r#abc",
            "o/r#0",
            "o/r#1x",
            "o/r#99999999999",
            "o#1",
            "o/r/x#1",
            "#1",
            "/o/r#1",
            "o/r #1",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/pull/abc",
            "https://github.com/o/pull/1",
            "https://github.com/o/r/pull/1/other",
            "https://github.com/o/r/pull/1/files/extra",
            "https://github.com/o r/pull/1",
            "https://github.com/%6f/r/pull/1",
            "https://github.com/o/r/pull/1\nhttps://evil.com",
            "https://github.com\\@evil.com/o/r/pull/1",
            "https://github.com?@evil.com/o/r/pull/1",
            "https://github.com/ö/r/pull/1",
            "gíthub.com/o/r#1",
        ] {
            assert!(
                matches!(
                    parse_locator(text),
                    Err(LocatorError::Malformed(_) | LocatorError::InvalidHost(_))
                ),
                "{text:?}: {:?}",
                parse_locator(text)
            );
        }
        assert!(matches!(
            FetchError::from(parse_locator("o/r#abc").unwrap_err()),
            FetchError::InvalidReference(_)
        ));
    }
}
