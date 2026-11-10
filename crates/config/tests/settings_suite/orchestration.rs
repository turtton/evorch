use config::{Config, LoadOptions};

#[test]
fn max_review_rounds_loads_configured_positive_value_and_rejects_zero() {
    let directory = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(directory.path().join(".evorch")).expect("config directory");
    let options = LoadOptions {
        user_config_dir: Some(directory.path().join(".evorch")),
        read_env: false,
        ..Default::default()
    };
    // Given: a configured review round limit / When: loading / Then: it is kept.
    std::fs::write(
        directory.path().join(".evorch/config.toml"),
        "[orchestration]\nmax_review_rounds = 5\n",
    )
    .expect("write");
    for load in [Config::load, Config::load_strict] {
        let config = load(&options).expect("positive limit");
        assert_eq!(config.orchestration.max_review_rounds.get(), 5);
    }

    // Given: a zero limit / When: loading / Then: reject instead of blocking every goal.
    std::fs::write(
        directory.path().join(".evorch/config.toml"),
        "[orchestration]\nmax_review_rounds = 0\n",
    )
    .expect("write");
    for load in [Config::load, Config::load_strict] {
        load(&options).expect_err("zero limit");
    }
}
