//! bubblewrap の実プロセス隔離を検証します。

use std::{
    fs,
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

use sandbox::{BwrapConfig, BwrapSandbox, CommandSpec, Sandbox, WrappedCommand};
use tempfile::{TempDir, tempdir, tempdir_in};

fn workspace() -> TempDir {
    tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("作業領域を作成できるはずです")
}

fn run(wrapped: WrappedCommand) -> Output {
    let mut command = Command::new(&wrapped.program);
    command.args(&wrapped.args).env_clear().envs(wrapped.env);
    if let Some(cwd) = wrapped.cwd {
        command.current_dir(cwd);
    }
    command.output().expect("隔離コマンドを起動できるはずです")
}

// Given: 親環境の秘密変数と作業領域外の資格情報ファイル / When: 隔離シェルから読む / Then: どちらも取得できない
#[ignore = "bwrap 実行環境が必要"]
#[test]
fn credentials_are_isolated() {
    if std::env::var_os("EVORCH_BWRAP_CHILD").is_none() {
        let output =
            Command::new(std::env::current_exe().expect("テスト実行ファイルを取得できるはずです"))
                .args([
                    "--exact",
                    "credentials_are_isolated",
                    "--nocapture",
                    "--include-ignored",
                ])
                .env("EVORCH_BWRAP_CHILD", "1")
                .env("FAKE_API_KEY", "parent-secret")
                .output()
                .expect("子テストを起動できるはずです");
        assert!(
            output.status.success(),
            "子テストが成功するはずです: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "子テストが実行されるはずです"
        );
        return;
    }

    let workspace = workspace();
    let sandbox = BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
        .expect("bwrap 実行環境が必要です");
    let outside = tempdir().expect("資格情報領域を作成できるはずです");
    let credential = outside.path().join("credentials.json");
    fs::write(&credential, "secret-file").expect("資格情報 fixture を書けるはずです");
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o600))
        .expect("権限を設定できるはずです");
    let script = format!(
        "cat '{}' 2>/dev/null || true; printf %s \"$FAKE_API_KEY\"",
        credential.display()
    );
    let wrapped = sandbox
        .wrap(CommandSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), script],
            cwd: None,
            extra_env: Vec::new(),
        })
        .expect("コマンドを包めるはずです");

    let output = run(wrapped);
    assert!(output.status.success(), "隔離シェルが成功するはずです");
    assert!(output.stdout.is_empty(), "資格情報が出力されないはずです");
}

// Given: 親名前空間のローカル TCP 待受 / When: 隔離内外から接続 / Then: 外では成功し隔離内では失敗する
#[ignore = "bwrap 実行環境が必要"]
#[test]
fn network_is_denied() {
    let workspace = workspace();
    let sandbox = BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
        .expect("bwrap 実行環境が必要です");
    let listener = TcpListener::bind("127.0.0.1:0").expect("ローカル待受を作成できるはずです");
    let address = listener
        .local_addr()
        .expect("待受アドレスを取得できるはずです");
    TcpStream::connect(address).expect("親名前空間から接続できるはずです");
    let script = format!("echo hi > /dev/tcp/127.0.0.1/{}", address.port());
    let wrapped = sandbox
        .wrap(CommandSpec {
            program: "bash".to_owned(),
            args: vec!["-c".to_owned(), script],
            cwd: None,
            extra_env: Vec::new(),
        })
        .expect("コマンドを包めるはずです");

    assert!(
        !run(wrapped).status.success(),
        "隔離名前空間からの接続は失敗するはずです"
    );
}

// Given: 書き込み可能な作業領域と外部パス / When: 両方へファイルを作成 / Then: 作業領域だけホストへ反映される
#[ignore = "bwrap 実行環境が必要"]
#[test]
fn writes_are_scoped_to_workspace() {
    let workspace = workspace();
    let sandbox = BwrapSandbox::detect(BwrapConfig::new(workspace.path().to_path_buf()))
        .expect("bwrap 実行環境が必要です");
    let inside = workspace.path().join("inside.txt");
    let outside = format!("/tmp/evorch-bwrap-test-{}", std::process::id());
    let script = format!(
        "printf inside > '{}'; printf outside > '{outside}'",
        inside.display()
    );
    let wrapped = sandbox
        .wrap(CommandSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), script],
            cwd: None,
            extra_env: Vec::new(),
        })
        .expect("コマンドを包めるはずです");

    let output = run(wrapped);
    assert!(
        output.status.success(),
        "隔離シェルが成功するはずです: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(inside).expect("作業領域のファイルを読めるはずです"),
        "inside"
    );
    assert!(
        !Path::new(&outside).exists(),
        "外部パスはホストへ反映されないはずです"
    );
}
