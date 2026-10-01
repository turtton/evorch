//! Host inheritance is explicit; isolated and unchecked launches keep filtering.
use std::{
    ffi::{OsStr, OsString},
    io::Read,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    process::Command,
};

use sandbox::{BwrapConfig, BwrapSandbox, CommandSpec, DirectSandbox, Sandbox, StdioSession};

const SECRET: &str = "opaque-fixture-credential-94376821";
const VARIABLES: &[(&str, &str)] = &[
    ("HOME", "/fixture/host-home"),
    ("GH_CONFIG_DIR", "/fixture/gh-config"),
    ("SSH_AUTH_SOCK", "/fixture/agent.sock"),
    ("HTTPS_PROXY", "http://fixture.invalid:8080"),
    ("EVORCH_CUSTOM_TOOL", "custom-tool-value"),
    ("GH_TOKEN", SECRET),
];

fn spec(script: &str) -> CommandSpec {
    CommandSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        extra_env: vec![],
    }
}

fn output(sandbox: &dyn Sandbox, spec: CommandSpec) -> Vec<u8> {
    let mut session = StdioSession::spawn(sandbox, spec).unwrap();
    let mut result = Vec::new();
    session
        .take_stdout()
        .unwrap()
        .read_to_end(&mut result)
        .unwrap();
    assert!(
        session
            .try_wait()
            .unwrap()
            .is_none_or(|exit| exit.success())
    );
    result
}

#[test]
fn host_environment_policy_preserves_native_values_and_explicit_overrides() {
    if std::env::var_os("EVORCH_ENV_FIXTURE_CHILD").is_none() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join(OsStr::from_bytes(b"bin-\xff"));
        std::fs::create_dir(&bin).unwrap();
        std::os::unix::fs::symlink("/bin/sh", bin.join("fixture-command")).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "host_environment_policy_preserves_native_values_and_explicit_overrides",
                "--nocapture",
            ])
            .env_clear()
            .env("EVORCH_ENV_FIXTURE_CHILD", "1")
            .env("PATH", &bin)
            .env("EVORCH_NON_UTF8", OsStr::from_bytes(b"native-\xff"))
            .env(
                OsStr::from_bytes(b"EVORCH_NATIVE_NAME_\xff"),
                "native-name-value",
            )
            .envs(VARIABLES.iter().copied());
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
        return;
    }

    let host = sandbox::composition::unsandboxed();
    let command = spec(
        "printf '%s\\n' \"$HOME\" \"$GH_CONFIG_DIR\" \"$SSH_AUTH_SOCK\" \"$HTTPS_PROXY\" \"$EVORCH_CUSTOM_TOOL\" \"$GH_TOKEN\"; printf '%s' \"$EVORCH_NON_UTF8\"",
    );
    let wrapped = host.wrap(command.clone()).unwrap();
    assert!(wrapped.inherit_env);
    assert!(
        wrapped.env.is_empty(),
        "inherited values must not be copied into metadata"
    );
    assert!(!format!("{wrapped:?}").contains(SECRET));
    let mut expected = VARIABLES
        .iter()
        .map(|(_, value)| format!("{value}\n"))
        .collect::<String>()
        .into_bytes();
    expected.extend_from_slice(b"native-\xff");
    assert_eq!(output(host.as_ref(), command), expected);

    // A native environment name also survives std Command inheritance. The
    // test binary, unlike a POSIX shell, does not discard invalid shell names.
    let mut native = spec("");
    native.program = std::env::current_exe().unwrap().to_str().unwrap().into();
    native.args = vec![
        "--exact".into(),
        "native_environment_name_probe".into(),
        "--nocapture".into(),
    ];
    native
        .extra_env
        .push(("EVORCH_NATIVE_NAME_PROBE".into(), "1".into()));
    assert!(String::from_utf8_lossy(&output(host.as_ref(), native)).contains("1 passed"));

    let mut override_spec = spec("printf '%s:%s' \"$HOME\" \"$EVORCH_CUSTOM_TOOL\"");
    override_spec.extra_env = vec![
        ("HOME".into(), "/override/home".into()),
        ("EVORCH_CUSTOM_TOOL".into(), "first".into()),
        ("EVORCH_CUSTOM_TOOL".into(), "last".into()),
    ];
    assert_eq!(output(host.as_ref(), override_spec), b"/override/home:last");

    // Preflight must resolve the same native PATH that the eventual child uses.
    let mut path_spec = spec("printf inherited-path");
    path_spec.program = "fixture-command".into();
    host.wrap(path_spec.clone()).unwrap().preflight().unwrap();
    assert_eq!(output(host.as_ref(), path_spec.clone()), b"inherited-path");
    path_spec.extra_env = vec![("PATH".into(), "/missing/override".into())];
    assert!(host.wrap(path_spec).unwrap().preflight().is_err());

    let direct = DirectSandbox::new_unchecked();
    assert!(!direct.wrap(spec("true")).unwrap().inherit_env);
    assert_eq!(
        output(
            &direct,
            spec("printf '%s' \"$GH_TOKEN$EVORCH_CUSTOM_TOOL$EVORCH_NON_UTF8\"")
        ),
        b""
    );

    // Both bwrap variants remain filtered and force their private HOME even
    // when explicit extra_env attempts to replace HOME.
    let temp = tempfile::tempdir().unwrap();
    let fake_bwrap = temp.path().join("bwrap");
    std::fs::write(&fake_bwrap, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake_bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
    let isolated =
        BwrapSandbox::detect_with_program(&fake_bwrap, BwrapConfig::new(temp.path().into()))
            .unwrap();
    let network = isolated.with_network_access().unwrap();
    for sandbox in [&isolated as &dyn Sandbox, network.as_ref()] {
        let mut command = spec("true");
        command
            .extra_env
            .push(("HOME".into(), "/override/home".into()));
        let wrapped = sandbox.wrap(command).unwrap();
        assert!(!wrapped.inherit_env);
        assert!(wrapped.env.contains(&("HOME".into(), "/tmp/home".into())));
        for name in [
            "GH_TOKEN",
            "GH_CONFIG_DIR",
            "SSH_AUTH_SOCK",
            "HTTPS_PROXY",
            "EVORCH_CUSTOM_TOOL",
            "EVORCH_NON_UTF8",
        ] {
            assert!(
                !wrapped.env.iter().any(|(key, _)| key == name),
                "unexpected inherited {name}"
            );
        }
    }
}

#[test]
fn native_environment_name_probe() {
    if std::env::var_os("EVORCH_NATIVE_NAME_PROBE").is_some() {
        assert_eq!(
            std::env::var_os(OsStr::from_bytes(b"EVORCH_NATIVE_NAME_\xff")),
            Some(OsString::from("native-name-value"))
        );
    }
}
