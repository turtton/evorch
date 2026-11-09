//! Project MCP configuration: loading both sources, precedence, expansion
//! grammar, validation and TOML-only saving. No environment is mutated: the
//! expansion API takes an explicit lookup.
use config::mcp::expand::{expand_template, parse_template, render_template};
use config::mcp::{
    McpConfigError, McpOverrideMutation, McpServerDefinition, McpSource, McpTransportDefinition,
    ProjectMcpConfig, SourceRevision, Template, TemplatePart, ValueSource, load_project,
    save_override,
};
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::TempDir;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn tpl(source: &str) -> Template {
    parse_template(source).unwrap()
}

fn stdio(command: &str) -> McpServerDefinition {
    McpServerDefinition {
        enabled: true,
        roles: vec!["worker".into()],
        transport: McpTransportDefinition::Stdio {
            command: tpl(command),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
        },
        startup_timeout_ms: 10_000,
        request_timeout_ms: 60_000,
    }
}

fn http(url: &str) -> McpServerDefinition {
    McpServerDefinition {
        enabled: true,
        roles: vec!["worker".into()],
        transport: McpTransportDefinition::Http {
            url: tpl(url),
            headers: BTreeMap::new(),
            token: None,
        },
        startup_timeout_ms: 10_000,
        request_timeout_ms: 60_000,
    }
}

fn upsert(id: &str, definition: McpServerDefinition) -> McpOverrideMutation {
    McpOverrideMutation::Upsert {
        server_id: id.into(),
        definition,
    }
}

fn load(root: &Path) -> ProjectMcpConfig {
    load_project(root).unwrap()
}

const JSON_FILES: &str = r#"{"mcpServers": {
  "files": {"command": "node", "args": ["./server.js"], "env": {"TOKEN": "${TOKEN}"}},
  "remote": {"type": "http", "url": "https://example.com/mcp",
             "headers": {"Authorization": "Bearer ${TOKEN}"}}
}}"#;

#[test]
fn mcp_json_is_never_written() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".mcp.json", JSON_FILES);
    let before = std::fs::read(dir.path().join(".mcp.json")).unwrap();

    let mut revision = load(dir.path()).revision;
    for mutation in [
        upsert("files", stdio("python3")),
        McpOverrideMutation::Disable {
            server_id: "remote".into(),
        },
        McpOverrideMutation::RemoveOverride {
            server_id: "files".into(),
        },
    ] {
        revision = save_override(dir.path(), revision, mutation)
            .unwrap()
            .revision;
    }
    assert_eq!(
        std::fs::read(dir.path().join(".mcp.json")).unwrap(),
        before,
        "imported JSON must stay byte-identical"
    );

    let empty = TempDir::new().unwrap();
    let revision = load(empty.path()).revision;
    save_override(empty.path(), revision, upsert("a", stdio("node"))).unwrap();
    assert!(
        !empty.path().join(".mcp.json").exists(),
        "saving must not create the imported file"
    );
    assert!(empty.path().join(".evorch/mcp.toml").is_file());
}

#[test]
fn json_import_defaults_and_unsupported_transports() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        ".mcp.json",
        r#"{"mcpServers": {
          "files": {"command": "node", "args": ["a"]},
          "legacy": {"type": "sse", "url": "https://example.com/sse"},
          "urlonly": {"url": "https://example.com/mcp"},
          "mixed": {"type": "http", "url": "https://example.com/mcp", "command": "node"},
          "extra": {"command": "node", "surprise": true}
        }}"#,
    );
    let config = load(dir.path());
    assert!(!config.preparation_blocked);
    let files = &config.servers["files"];
    assert_eq!(files.source, McpSource::ImportedJson);
    let definition = files.definition.as_ref().unwrap();
    assert!(definition.enabled);
    assert_eq!(definition.roles, ["worker"]);
    for id in ["legacy", "urlonly", "mixed", "extra"] {
        let entry = &config.servers[id];
        assert!(entry.definition.is_none(), "{id} must not be guessed");
        assert_eq!(entry.diagnostics.len(), 1, "{id}");
        assert_eq!(entry.diagnostics[0].server_id.as_deref(), Some(id));
    }
}

#[test]
fn toml_replaces_whole_server() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".mcp.json", JSON_FILES);
    write(
        dir.path(),
        ".evorch/mcp.toml",
        r#"version = 1

[servers.files]
type = "http"
url = "http://127.0.0.1:3000/mcp"
roles = ["reviewer"]
"#,
    );
    let config = load(dir.path());
    let files = &config.servers["files"];
    assert_eq!(files.source, McpSource::ProjectToml);
    assert!(files.imported_exists);
    assert_eq!(
        files.definition,
        Some(McpServerDefinition {
            roles: vec!["reviewer".into()],
            ..http("http://127.0.0.1:3000/mcp")
        }),
        "no env/args/command from the imported definition may survive"
    );
    let remote = &config.servers["remote"];
    assert_eq!(remote.source, McpSource::ImportedJson);
    assert!(remote.definition.is_some());
}

#[test]
fn disabled_override_does_not_fall_back() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".mcp.json", JSON_FILES);
    write(
        dir.path(),
        ".evorch/mcp.toml",
        r#"version = 1

[servers.files]
enabled = false

[servers.remote]
type = "http"
url = "ftp://not-allowed"
"#,
    );
    let config = load(dir.path());
    assert!(!config.preparation_blocked);

    let tombstone = &config.servers["files"];
    assert!(tombstone.tombstone && tombstone.definition.is_none());
    assert!(tombstone.imported_exists && tombstone.diagnostics.is_empty());

    let broken = &config.servers["remote"];
    assert!(!broken.tombstone);
    assert!(
        broken.definition.is_none(),
        "invalid override must not revert"
    );
    assert_eq!(broken.diagnostics.len(), 1);
    assert_eq!(broken.source, McpSource::ProjectToml);
}

#[test]
fn broken_toml_blocks_preparation_without_falling_back() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".mcp.json", JSON_FILES);
    for broken in [
        "version = 1\n[servers.files\n",
        "version = 2\n",
        "[servers.files]\nenabled = false\n",
        "version = 1\nsurprise = true\n",
        "version = 1\n[servers.a]\nenabled = false\n[servers.a]\nenabled = false\n",
    ] {
        write(dir.path(), ".evorch/mcp.toml", broken);
        let config = load(dir.path());
        assert!(config.preparation_blocked, "{broken:?}");
        assert!(config.servers.is_empty(), "{broken:?}");
        assert_eq!(config.diagnostics.len(), 1);
        assert_eq!(config.diagnostics[0].source, McpSource::ProjectToml);
    }
}

#[test]
fn duplicate_json_keys_are_rejected() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        ".mcp.json",
        r#"{"mcpServers": {"a": {"command": "x"}, "a": {"command": "y"}}}"#,
    );
    let config = load(dir.path());
    assert!(config.servers.is_empty());
    assert_eq!(config.diagnostics[0].code, McpConfigError::Syntax);
    assert!(!config.preparation_blocked);
}

#[test]
fn expansion_default_for_unset_or_empty() {
    let template = tpl("a-${V:-fallback}-b");
    let expand = |value: Option<&'static str>| {
        expand_template(&template, &|name| {
            assert_eq!(name, "V");
            value.map(str::to_owned)
        })
    };
    assert_eq!(expand(None).unwrap(), "a-fallback-b");
    assert_eq!(expand(Some("")).unwrap(), "a-fallback-b");
    assert_eq!(expand(Some("set")).unwrap(), "a-set-b");

    let plain = tpl("${V}");
    assert_eq!(
        expand_template(&plain, &|_| None),
        Err(McpConfigError::MissingEnvironment)
    );
    assert_eq!(
        expand_template(&plain, &|_| Some(String::new())).unwrap(),
        ""
    );
    assert_eq!(
        expand_template(&tpl("${V:-}"), &|_| None).unwrap(),
        "",
        "an empty default is a default"
    );

    // The same grammar applies to both source files.
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        ".mcp.json",
        r#"{"mcpServers": {"j": {"command": "${CMD:-node}"}}}"#,
    );
    write(
        dir.path(),
        ".evorch/mcp.toml",
        "version = 1\n[servers.t]\ntype = \"stdio\"\ncommand = \"${CMD:-node}\"\n",
    );
    let config = load(dir.path());
    let command = |id: &str| match &config.servers[id].definition.as_ref().unwrap().transport {
        McpTransportDefinition::Stdio { command, .. } => command.clone(),
        McpTransportDefinition::Http { .. } => unreachable!(),
    };
    assert_eq!(command("j"), command("t"));
    assert_eq!(
        command("j"),
        Template(vec![TemplatePart::Env {
            name: "CMD".into(),
            default: Some("node".into())
        }]),
        "references stay structured for approval fingerprints"
    );
}

#[test]
fn expansion_is_non_recursive() {
    let lookup = |name: &str| match name {
        "A" => Some("${B}".to_owned()),
        "B" => Some("boom".to_owned()),
        _ => None,
    };
    assert_eq!(expand_template(&tpl("${A}"), &lookup).unwrap(), "${B}");
    assert_eq!(
        parse_template("${X:-${B}}"),
        Err(McpConfigError::InvalidTemplate),
        "a reference inside a default is nesting"
    );

    assert_eq!(
        expand_template(&tpl("$${A} ${A}"), &lookup).unwrap(),
        "${A} ${B}"
    );
    assert_eq!(
        tpl("$${A}"),
        Template(vec![TemplatePart::Literal {
            value: "${A}".into()
        }])
    );
    for literal in ["$HOME", "cost $5", "$$", "$", "a$"] {
        assert_eq!(expand_template(&tpl(literal), &lookup).unwrap(), literal);
    }
    for malformed in ["${", "${A", "${}", "${1A}", "${A B}", "${A:-x"] {
        assert_eq!(
            parse_template(malformed),
            Err(McpConfigError::InvalidTemplate),
            "{malformed}"
        );
    }
}

#[test]
fn template_source_round_trips() {
    for source in [
        "plain",
        "${A}",
        "${A:-d}",
        "x${A:-}y",
        "$${A}",
        "$$${A}",
        "cost $5 $$",
        "https://${H:-localhost}:${P:-80}/mcp",
    ] {
        let template = tpl(source);
        assert_eq!(
            tpl(&render_template(&template).unwrap()),
            template,
            "{source}"
        );
    }
    let escape_hazard = Template(vec![
        TemplatePart::Literal { value: "a$".into() },
        TemplatePart::Env {
            name: "X".into(),
            default: None,
        },
    ]);
    assert_eq!(
        render_template(&escape_hazard),
        Err(McpConfigError::InvalidTemplate)
    );
}

#[test]
fn save_rejects_stale_source_revision() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".mcp.json", JSON_FILES);
    let revision = load(dir.path()).revision;

    write(dir.path(), ".mcp.json", r#"{"mcpServers": {}}"#);
    assert_eq!(
        save_override(dir.path(), revision, upsert("a", stdio("node"))).unwrap_err(),
        McpConfigError::Conflict
    );
    assert!(!dir.path().join(".evorch/mcp.toml").exists());

    let revision = load(dir.path()).revision;
    save_override(dir.path(), revision, upsert("a", stdio("node"))).unwrap();
    assert_eq!(
        save_override(dir.path(), revision, upsert("b", stdio("node"))).unwrap_err(),
        McpConfigError::Conflict,
        "an already-consumed revision is stale"
    );
    assert_eq!(
        save_override(
            dir.path(),
            SourceRevision::from_sources(None, None),
            upsert("b", stdio("node"))
        )
        .unwrap_err(),
        McpConfigError::Conflict
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".evorch"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, ["mcp.toml"], "no temp files may remain");
}

#[cfg(unix)]
#[test]
fn save_rejects_symlink() {
    use std::os::unix::fs::symlink;
    let outside = TempDir::new().unwrap();
    write(outside.path(), "target.toml", "version = 1\n");

    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join(".evorch")).unwrap();
    symlink(
        outside.path().join("target.toml"),
        dir.path().join(".evorch/mcp.toml"),
    )
    .unwrap();
    let revision = load(dir.path()).revision;
    assert_eq!(
        save_override(dir.path(), revision, upsert("a", stdio("node"))).unwrap_err(),
        McpConfigError::Symlink
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("target.toml")).unwrap(),
        "version = 1\n"
    );

    let dir = TempDir::new().unwrap();
    symlink(outside.path(), dir.path().join(".evorch")).unwrap();
    let revision = load(dir.path()).revision;
    assert_eq!(
        save_override(dir.path(), revision, upsert("a", stdio("node"))).unwrap_err(),
        McpConfigError::Symlink
    );
    assert!(!outside.path().join("mcp.toml").exists());
}

#[cfg(unix)]
#[test]
fn saved_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let revision = load(dir.path()).revision;
    save_override(dir.path(), revision, upsert("a", stdio("node"))).unwrap();
    let mode = std::fs::metadata(dir.path().join(".evorch/mcp.toml"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn upsert_round_trips_complete_definitions() {
    let dir = TempDir::new().unwrap();
    let stdio_full = McpServerDefinition {
        enabled: false,
        roles: vec!["worker".into(), "web_researcher".into()],
        transport: McpTransportDefinition::Stdio {
            command: tpl("${NODE:-node}"),
            args: vec![tpl("./server.js"), tpl("--cost=$5"), tpl("$${LITERAL}")],
            cwd: Some(tpl("./sub")),
            env: BTreeMap::from([
                (
                    "MODE".into(),
                    ValueSource::Template {
                        template: tpl("development"),
                    },
                ),
                (
                    "KEY".into(),
                    ValueSource::Keychain {
                        name: "files-token".into(),
                    },
                ),
                (
                    "REGION".into(),
                    ValueSource::Template {
                        template: tpl("${MCP_REGION:-local}"),
                    },
                ),
            ]),
        },
        startup_timeout_ms: 1,
        request_timeout_ms: 600_000,
    };
    let http_full = McpServerDefinition {
        enabled: true,
        roles: Vec::new(),
        transport: McpTransportDefinition::Http {
            url: tpl("https://mcp.example.com:8443/mcp?x=1"),
            headers: BTreeMap::from([(
                "X-Tenant".into(),
                ValueSource::Template {
                    template: tpl("${MCP_TENANT:-default}"),
                },
            )]),
            token: Some(ValueSource::Keychain {
                name: "remote-token".into(),
            }),
        },
        startup_timeout_ms: 10_000,
        request_timeout_ms: 60_000,
    };
    let mut revision = load(dir.path()).revision;
    for (id, definition) in [("files", &stdio_full), ("remote", &http_full)] {
        let saved = save_override(dir.path(), revision, upsert(id, definition.clone())).unwrap();
        revision = saved.revision;
        assert!(saved.servers[id].diagnostics.is_empty(), "{id}");
        assert_eq!(
            saved.servers[id].definition.as_ref(),
            Some(definition),
            "{id}"
        );
    }
    assert_eq!(load(dir.path()).revision, revision);
    assert_eq!(
        load(dir.path()).servers["remote"]
            .definition
            .as_ref()
            .unwrap()
            .roles,
        Vec::<String>::new(),
        "an empty role list stays unpublished"
    );
}

#[test]
fn disable_and_remove_override_are_distinct_and_keep_other_text() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".mcp.json", JSON_FILES);
    write(
        dir.path(),
        ".evorch/mcp.toml",
        "# my notes\nversion = 1\n\n[servers.keep]\ntype = \"stdio\"\ncommand = \"keep\" # inline\n",
    );
    let revision = load(dir.path()).revision;
    let disabled = save_override(
        dir.path(),
        revision,
        McpOverrideMutation::Disable {
            server_id: "files".into(),
        },
    )
    .unwrap();
    assert!(disabled.servers["files"].tombstone);
    let text = std::fs::read_to_string(dir.path().join(".evorch/mcp.toml")).unwrap();
    assert!(text.contains("# my notes") && text.contains("# inline"));
    assert!(disabled.servers["keep"].definition.is_some());

    let revealed = save_override(
        dir.path(),
        disabled.revision,
        McpOverrideMutation::RemoveOverride {
            server_id: "files".into(),
        },
    )
    .unwrap();
    let files = &revealed.servers["files"];
    assert_eq!(files.source, McpSource::ImportedJson);
    assert!(!files.tombstone && files.definition.is_some());

    let unchanged = std::fs::read(dir.path().join(".evorch/mcp.toml")).unwrap();
    let again = save_override(
        dir.path(),
        revealed.revision,
        McpOverrideMutation::RemoveOverride {
            server_id: "files".into(),
        },
    )
    .unwrap();
    assert_eq!(again.revision, revealed.revision);
    assert_eq!(
        std::fs::read(dir.path().join(".evorch/mcp.toml")).unwrap(),
        unchanged
    );
}

#[test]
fn save_refuses_to_overwrite_a_broken_document_or_invalid_definition() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".evorch/mcp.toml", "version = 1\n[servers.a\n");
    let before = std::fs::read(dir.path().join(".evorch/mcp.toml")).unwrap();
    let revision = load(dir.path()).revision;
    assert_eq!(
        save_override(dir.path(), revision, upsert("b", stdio("node"))).unwrap_err(),
        McpConfigError::Syntax
    );
    assert_eq!(
        std::fs::read(dir.path().join(".evorch/mcp.toml")).unwrap(),
        before
    );

    let clean = TempDir::new().unwrap();
    let revision = load(clean.path()).revision;
    let mut bad = stdio("node");
    bad.roles = vec!["nobody".into()];
    assert_eq!(
        save_override(clean.path(), revision, upsert("a", bad)).unwrap_err(),
        McpConfigError::Invalid
    );
    assert!(!clean.path().join(".evorch").exists());
}

fn toml_server(body: &str) -> ProjectMcpConfig {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        ".evorch/mcp.toml",
        &format!("version = 1\n[servers.s]\n{body}\n"),
    );
    load(dir.path())
}

#[test]
fn validation_rejects_bad_definitions() {
    let invalid = [
        // missing type / transport fields / exclusivity
        "command = \"node\"",
        "type = \"stdio\"",
        "type = \"http\"",
        "type = \"stdio\"\ncommand = \"node\"\nurl = \"https://x.example\"",
        "type = \"http\"\nurl = \"https://x.example\"\nargs = []",
        "type = \"sse\"\nurl = \"https://x.example\"",
        // unknown field / role / duplicate role / wrong types
        "type = \"stdio\"\ncommand = \"node\"\nsurprise = 1",
        "type = \"stdio\"\ncommand = \"node\"\nroles = [\"nobody\"]",
        "type = \"stdio\"\ncommand = \"node\"\nroles = [\"worker\", \"worker\"]",
        "type = \"stdio\"\ncommand = \"node\"\nroles = \"worker\"",
        "type = \"stdio\"\ncommand = \"node\"\nenabled = \"yes\"",
        // command
        "type = \"stdio\"\ncommand = \"\"",
        "type = \"stdio\"\ncommand = \"a\\u0000b\"",
        "type = \"stdio\"\ncommand = { keychain = \"k\" }",
        "type = \"stdio\"\ncommand = \"node\"\nargs = [{ keychain = \"k\" }]",
        "type = \"stdio\"\ncommand = \"node\"\ncwd = { keychain = \"k\" }",
        // env
        "type = \"stdio\"\ncommand = \"node\"\n[servers.s.env]\n\"A=B\" = \"x\"",
        "type = \"stdio\"\ncommand = \"node\"\n[servers.s.env]\n\"\" = \"x\"",
        "type = \"stdio\"\ncommand = \"node\"\n[servers.s.env]\nA = 1",
        "type = \"stdio\"\ncommand = \"node\"\n[servers.s.env]\nA = { keychain = \"\" }",
        "type = \"stdio\"\ncommand = \"node\"\n[servers.s.env]\nA = { keychain = \"k\", other = \"x\" }",
        // timeouts
        "type = \"stdio\"\ncommand = \"node\"\nstartup_timeout_ms = 0",
        "type = \"stdio\"\ncommand = \"node\"\nrequest_timeout_ms = 600001",
        "type = \"stdio\"\ncommand = \"node\"\nrequest_timeout_ms = -5",
        // url
        "type = \"http\"\nurl = \"ftp://x.example/mcp\"",
        "type = \"http\"\nurl = \"https://user:pw@x.example/mcp\"",
        "type = \"http\"\nurl = \"https://x.example/mcp#frag\"",
        "type = \"http\"\nurl = \"https://x.example:0/mcp\"",
        "type = \"http\"\nurl = \"https://x.example:99999/mcp\"",
        "type = \"http\"\nurl = \"https://x.example:/mcp\"",
        "type = \"http\"\nurl = \"https:///mcp\"",
        "type = \"http\"\nurl = \"https://x .example/mcp\"",
        "type = \"http\"\nurl = \"x.example/mcp\"",
        // headers / token
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nHost = \"evil\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nmcp-method = \"x\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nMcp-Param-Foo = \"x\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nContent-Length = \"1\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nAccept-Encoding = \"gzip\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\naCcEpT-EnCoDiNg = \"identity\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\n\"Bad Name\" = \"x\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nX-A = \"a\\r\\nX-B: b\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nX-A = \"1\"\nx-a = \"2\"",
        "type = \"http\"\nurl = \"https://x.example\"\ntoken = \"t\"\n[servers.s.headers]\nauthorization = \"Bearer x\"",
    ];
    for body in invalid {
        let config = toml_server(body);
        assert!(!config.preparation_blocked, "{body}");
        let entry = &config.servers["s"];
        assert!(entry.definition.is_none(), "should be rejected: {body}");
        assert_eq!(entry.diagnostics.len(), 1, "{body}");
    }

    let valid = [
        "type = \"stdio\"\ncommand = \"node\"",
        "type = \"stdio\"\ncommand = \"node\"\nenabled = false\nroles = []",
        "type = \"http\"\nurl = \"http://[::1]:3000/mcp\"",
        "type = \"http\"\nurl = \"${MCP_URL}\"",
        "type = \"http\"\nurl = \"https://${HOST:-example.com}:${PORT:-443}/mcp\"",
        "type = \"http\"\nurl = \"https://x.example/mcp?token=a&b=c\"",
        "type = \"http\"\nurl = \"https://x.example\"\n[servers.s.headers]\nAuthorization = \"Bearer ${T}\"",
        "type = \"http\"\nurl = \"https://x.example\"\ntoken = { keychain = \"t\" }\n[servers.s.headers]\nX-Tenant = \"a\"",
    ];
    for body in valid {
        let entry = toml_server(body).servers.remove("s").unwrap();
        assert!(entry.definition.is_some(), "should be accepted: {body}");
    }
}

#[test]
fn limits_are_classified_failures() {
    let dir = TempDir::new().unwrap();

    let mut many = String::from("version = 1\n");
    for index in 0..=config::mcp::MAX_SERVERS {
        many.push_str(&format!("[servers.s{index}]\nenabled = false\n"));
    }
    write(dir.path(), ".evorch/mcp.toml", &many);
    let config = load(dir.path());
    assert!(config.preparation_blocked);
    assert_eq!(config.diagnostics[0].code, McpConfigError::LimitExceeded);

    let at_limit = many.replace("[servers.s64]\nenabled = false\n", "");
    write(dir.path(), ".evorch/mcp.toml", &at_limit);
    let config = load(dir.path());
    assert!(!config.preparation_blocked);
    assert_eq!(config.servers.len(), config::mcp::MAX_SERVERS);

    // JSON and TOML each within the limit but the union above it.
    let json: String = (0..2)
        .map(|index| format!("\"j{index}\": {{\"command\": \"x\"}}"))
        .collect::<Vec<_>>()
        .join(",");
    write(
        dir.path(),
        ".mcp.json",
        &format!("{{\"mcpServers\": {{{json}}}}}"),
    );
    let config = load(dir.path());
    assert!(config.preparation_blocked);
    assert_eq!(config.diagnostics[0].code, McpConfigError::LimitExceeded);

    let oversized = "x".repeat(config::mcp::MAX_CONFIG_BYTES + 1);
    write(dir.path(), ".mcp.json", &oversized);
    write(dir.path(), ".evorch/mcp.toml", "version = 1\n");
    let config = load(dir.path());
    assert!(!config.preparation_blocked);
    assert_eq!(config.diagnostics[0].code, McpConfigError::LimitExceeded);
    assert_eq!(config.diagnostics[0].source, McpSource::ImportedJson);

    let long = format!(
        "version = 1\n[servers.s]\ntype = \"stdio\"\ncommand = \"{}\"\n",
        "c".repeat(config::mcp::MAX_STRING_BYTES + 1)
    );
    write(dir.path(), ".mcp.json", "{}");
    write(dir.path(), ".evorch/mcp.toml", &long);
    let config = load(dir.path());
    assert_eq!(
        config.servers["s"].diagnostics[0].code,
        McpConfigError::LimitExceeded
    );
}

#[test]
fn absent_sources_load_empty_and_roles_default_to_worker() {
    let dir = TempDir::new().unwrap();
    let config = load(dir.path());
    assert!(config.servers.is_empty() && config.diagnostics.is_empty());
    assert!(!config.preparation_blocked);
    assert_eq!(config.revision, SourceRevision::from_sources(None, None));

    let config = toml_server("type = \"stdio\"\ncommand = \"node\"");
    let definition = config.servers["s"].definition.clone().unwrap();
    assert_eq!(definition.roles, ["worker"]);
    assert_eq!(
        definition.startup_timeout_ms,
        config::mcp::DEFAULT_STARTUP_TIMEOUT_MS
    );
    assert_eq!(
        definition.request_timeout_ms,
        config::mcp::DEFAULT_REQUEST_TIMEOUT_MS
    );
}
