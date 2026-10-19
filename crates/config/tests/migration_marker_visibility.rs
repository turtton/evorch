//! Migration provenance must not become user-facing diagnostics or configuration.

use config::{Config, LoadOptions};

#[derive(Clone, Default)]
struct LogBuffer(std::sync::Arc<std::sync::Mutex<String>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// Same thread-local capture as storage/tests/undecodable_events.rs: no global subscriber.
fn capture_logs<T>(action: impl FnOnce() -> T) -> (T, String) {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    let result = tracing::subscriber::with_default(subscriber, action);
    let logs = buffer.0.lock().expect("log buffer lock").clone();
    (result, logs)
}

fn assert_no_markers(text: &str) {
    assert!(
        !text.contains("__evorch_"),
        "internal marker leaked: {text}"
    );
}

fn assert_project_route_warning(route: &str, effort: &str) {
    for file in ["config.toml", "config.d/10-route.toml"] {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let path = project.join(".evorch").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "role_profile = 'consult'\n[routing.routes]\n{route} = [{{ profile = 'p'{effort} }}]\n"
            ),
        )
        .unwrap();
        let options = LoadOptions {
            project_dir: Some(project.clone()),
            user_config_dir: Some(directory.path().join("user")),
            read_env: false,
            ..Default::default()
        };
        let (loaded, logs) = capture_logs(|| Config::load_strict(&options));
        let loaded = loaded.unwrap();
        assert_eq!(loaded.role_profile.as_deref(), Some("consult"));
        assert!(!loaded.routing.routes.contains_key(route));
        assert!(logs.contains("ignoring project config field"), "{logs}");
        assert!(logs.contains("field=routing"), "{logs}");
        assert_no_markers(&logs);

        // The project selector used by GUI settings shares the same warning seam.
        let (selected, logs) = capture_logs(|| config::project_role_profile(&project));
        assert_eq!(selected.unwrap().as_deref(), Some("consult"));
        assert!(logs.contains("field=routing"), "{logs}");
        assert_no_markers(&logs);
    }
}

#[test]
fn project_advisor_route_warning_has_no_internal_markers() {
    assert_project_route_warning("advisor", "");
}

#[test]
fn project_oracle_effort_route_warning_has_no_internal_markers() {
    assert_project_route_warning("oracle", ", reasoning_effort = 'high'");
}

#[test]
fn project_user_key_with_internal_prefix_is_still_reported() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let path = project.join(".evorch/config.toml");
    // file_overrides is also used by GUI save previews.
    let options = LoadOptions {
        project_dir: Some(project),
        user_config_dir: Some(directory.path().join("user")),
        file_overrides: [(
            path,
            toml::from_str(
                "__evorch_user_typo = true\n[routing.routes]\nadvisor = [{ profile = 'p' }]\n",
            )
            .unwrap(),
        )]
        .into(),
        read_env: false,
        ..Default::default()
    };
    let (loaded, logs) = capture_logs(|| Config::load_strict(&options));
    loaded.unwrap();
    assert!(logs.contains("field=__evorch_user_typo"), "{logs}");
    assert!(logs.contains("field=routing"), "{logs}");
    assert!(!logs.contains("__evorch_implicit_"), "{logs}");
}

#[test]
fn loaded_display_gui_and_saved_values_have_no_internal_markers() {
    for route in ["advisor", "oracle"] {
        for scope in ["", "role_profiles.consult."] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            let source = format!(
                "[{scope}routing.routes]\n{route} = [{{ profile = 'p', reasoning_effort = 'high' }}]\n"
            );
            std::fs::write(&path, &source).unwrap();
            let options = LoadOptions {
                user_config_dir: Some(directory.path().to_path_buf()),
                read_env: false,
                ..Default::default()
            };
            for load in [Config::load, Config::load_strict, Config::load_unresolved] {
                let (loaded, logs) = capture_logs(|| load(&options));
                let loaded = loaded.unwrap();
                assert_no_markers(&logs);
                assert_no_markers(&format!("{loaded:?}"));
                assert_no_markers(&toml::to_string(&loaded).unwrap());
                assert_no_markers(&serde_json::to_string(&loaded).unwrap());
            }
            let preview = Config::load_unresolved(&LoadOptions {
                file_overrides: [(path.clone(), toml::from_str(&source).unwrap())].into(),
                ..options
            })
            .unwrap();
            assert_no_markers(&toml::to_string(&preview).unwrap());
            config::save_sandbox(&path, config::SandboxConfig::default()).unwrap();
            assert_no_markers(&std::fs::read_to_string(&path).unwrap());
            // v2 upgrades write migrated sections, unlike current-format validation.
            std::fs::write(&path, format!("version = 2\n{source}")).unwrap();
            config::save_sandbox(&path, config::SandboxConfig::default()).unwrap();
            assert_no_markers(&std::fs::read_to_string(&path).unwrap());
        }
    }
}

#[test]
fn invalid_migrated_config_errors_have_no_internal_markers() {
    for route in ["advisor", "oracle"] {
        for scope in ["", "role_profiles.consult."] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            std::fs::write(
                &path,
                format!(
                    "[{scope}routing.routes]\n{route} = [{{ profile = 42, reasoning_effort = 'high' }}]\n"
                ),
            )
            .unwrap();
            let options = LoadOptions {
                user_config_dir: Some(directory.path().to_path_buf()),
                read_env: false,
                ..Default::default()
            };
            for load in [Config::load, Config::load_strict, Config::load_unresolved] {
                let (result, logs) = capture_logs(|| load(&options));
                let error = result.unwrap_err();
                assert_no_markers(&logs);
                assert_no_markers(&error.to_string());
                assert_no_markers(&format!("{error:?}"));
            }
            let error = config::save_sandbox(&path, config::SandboxConfig::default()).unwrap_err();
            assert_no_markers(&error.to_string());
            assert_no_markers(&format!("{error:?}"));
        }
    }
}

#[test]
fn user_marker_names_warn_or_fail_without_replacing_explicit_chains() {
    for marker in [
        "__evorch_implicit_advisor_route",
        "__evorch_implicit_oracle_chain",
    ] {
        for marker_value in ["false", "true", "'advisor'"] {
            for scope in ["", "role_profiles.consult."] {
                for role in ["oracle", "advisor"] {
                    for location in ["scope", "route", "models"] {
                        let directory = tempfile::tempdir().unwrap();
                        let path = directory.path().join("config.toml");
                        let field = format!("{marker} = {marker_value}");
                        let scope_field = if location == "scope" {
                            if scope.is_empty() {
                                format!("{field}\n")
                            } else {
                                format!("[role_profiles.consult]\n{field}\n")
                            }
                        } else {
                            String::new()
                        };
                        let route_field = if location == "route" {
                            format!(", {field}")
                        } else {
                            String::new()
                        };
                        let model_field = if location == "models" {
                            format!(", {field}")
                        } else {
                            String::new()
                        };
                        let source = format!(
                            "{scope_field}[{scope}routing.routes]\ncustom = [{{ profile = 'p'{route_field} }}]\n\
                             [{scope}agents.roles.{role}]\nmodels = [{{ route = 'custom', reasoning_effort = 'low'{model_field} }}]\n"
                        );
                        // An upper Advisor route must not turn a user model into a generated one.
                        std::fs::create_dir(directory.path().join("config.d")).unwrap();
                        std::fs::write(directory.path().join("config.d/10-advisor.toml"), format!(
                            "[{scope}routing.routes]\nadvisor = [{{ profile = 'p', reasoning_effort = 'high' }}]\n"
                        )).unwrap();
                        let expected_path = match location {
                            "scope" => format!("{scope}{marker}"),
                            "route" => format!("{scope}routing.routes.custom[0].{marker}"),
                            _ => format!("{scope}agents.roles.advisor.models[0].{marker}"),
                        };
                        for preview in [false, true] {
                            let mut options = LoadOptions {
                                user_config_dir: Some(directory.path().to_path_buf()),
                                read_env: false,
                                ..Default::default()
                            };
                            if preview {
                                options
                                    .file_overrides
                                    .insert(path.clone(), toml::from_str(&source).unwrap());
                            } else {
                                std::fs::write(&path, &source).unwrap();
                            }
                            for load in [Config::load, Config::load_unresolved] {
                                let (loaded, logs) = capture_logs(|| load(&options));
                                let loaded = loaded.unwrap();
                                assert!(logs.contains("ignoring unknown config field"), "{logs}");
                                assert!(logs.contains(&format!("field={expected_path}")), "{logs}");
                                let agents = if scope.is_empty() {
                                    &loaded.agents
                                } else {
                                    &loaded.role_profiles["consult"].agents
                                };
                                let chain = agents.binding_for("advisor", None).unwrap().models;
                                assert_eq!(chain.len(), 1);
                                assert_eq!(chain[0].route, "custom", "{source}");
                                assert_eq!(chain[0].reasoning_effort.as_deref(), Some("low"));
                            }
                            match Config::load_strict(&options).unwrap_err() {
                                config::ConfigError::InvalidField { path, message } => {
                                    assert_eq!(path, expected_path);
                                    assert!(message.contains("unknown field"), "{message}");
                                }
                                error => panic!("unexpected error: {error}"),
                            }
                        }
                        // Saving must reject the user's field, rather than consuming it as metadata.
                        let before = std::fs::read_to_string(&path).unwrap();
                        assert!(
                            config::save_sandbox(&path, config::SandboxConfig::default()).is_err()
                        );
                        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
                    }
                }
            }
        }
    }
}

#[test]
fn marker_names_remain_legal_user_route_and_profile_identifiers() {
    let directory = tempfile::tempdir().unwrap();
    let source = "[routing.routes]\n\
        __evorch_implicit_advisor_route = [{ profile = 'p' }]\n\
        [agents.roles.advisor]\n\
        models = [{ route = '__evorch_implicit_advisor_route' }]\n\
        [role_profiles.__evorch_implicit_oracle_chain.routing.routes]\n\
        __evorch_implicit_oracle_chain = [{ profile = 'p' }]\n\
        [role_profiles.__evorch_implicit_oracle_chain.agents.roles.advisor]\n\
        models = [{ route = '__evorch_implicit_oracle_chain' }]\n";
    std::fs::write(directory.path().join("config.toml"), source).unwrap();
    let (loaded, logs) = capture_logs(|| {
        Config::load_strict(&LoadOptions {
            user_config_dir: Some(directory.path().to_path_buf()),
            read_env: false,
            ..Default::default()
        })
    });
    let loaded = loaded.unwrap();
    assert!(logs.is_empty(), "{logs}");
    assert!(
        loaded
            .routing
            .routes
            .contains_key("__evorch_implicit_advisor_route")
    );
    assert!(
        loaded.role_profiles["__evorch_implicit_oracle_chain"]
            .routing
            .routes
            .contains_key("__evorch_implicit_oracle_chain")
    );
}
