#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn tilde_alone_expands_to_home() {
        // Given / When / Then: a home directory resolves a bare tilde.
        assert_eq!(expand_tilde(Path::new("~"), Some(Path::new("/home/test"))), Ok("/home/test".into()));
    }

    #[test]
    fn tilde_slash_expands() {
        // Given / When / Then: the suffix remains under the supplied home.
        assert_eq!(expand_tilde(Path::new("~/repo"), Some(Path::new("/home/test"))), Ok("/home/test/repo".into()));
    }

    #[test]
    fn tilde_user_is_unsupported() {
        // Given / When / Then: named-user expansion is deliberately unsupported.
        assert_eq!(expand_tilde(Path::new("~other/repo"), Some(Path::new("/home/test"))), Err(ProjectPathError::Unsupported));
    }

    #[test]
    fn absolute_path_untouched() {
        // Given / When / Then: absolute paths do not require a home.
        assert_eq!(expand_tilde(Path::new("/repo"), None), Ok("/repo".into()));
    }

    #[test]
    fn empty_input_rejected() {
        // Given / When / Then: empty input is not the working directory.
        assert_eq!(expand_tilde(Path::new(""), None), Err(ProjectPathError::Empty));
    }

    #[test]
    fn no_home_errors() {
        // Given / When / Then: missing home produces an actionable typed error.
        assert_eq!(expand_tilde(Path::new("~/repo"), None), Err(ProjectPathError::NoHome));
    }
}
