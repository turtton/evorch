//! ネットワーク境界からサンドボックス実行モードへのマッピング (issue #19 / ADR 0021)。
//!
//! [`NetworkAccess`] (ADR 0002 の capability boundary) を `SandboxNetworkMode`
//! (bwrap の netns 実行形態) へ解決する純粋変換と [`ExecutionPolicy`] 経由の
//! 解決を検証する。bwrap 実行環境が必要な `build_sandbox` を経由した伝達は
//! ignore 付きの統合テストで検証する。

use std::{
    net::{TcpListener, TcpStream},
    process::{Command, Output},
};

use agents::{NetworkAccess, Role, RoleCapabilities};
use runtime::{ExecutionPolicy, SandboxNetworkMode, build_sandbox, sandbox_network_mode};
use sandbox::{CommandSpec, WrappedCommand};
use tempfile::tempdir_in;

fn run(wrapped: WrappedCommand) -> Output {
    let mut command = Command::new(&wrapped.program);
    command.args(&wrapped.args).env_clear().envs(wrapped.env);
    if let Some(cwd) = wrapped.cwd {
        command.current_dir(cwd);
    }
    command.output().expect("隔離コマンドを起動できるはずです")
}

// Given: NetworkAccess::Allowed (ロール定義レベルで常に許可)
// When: explicit_opt_in = false でマッピングする
// Then: ParentNetns に解決される
#[test]
fn allowed_without_explicit_opt_in_maps_to_parent_netns() {
    let mode = sandbox_network_mode(NetworkAccess::Allowed, false);

    assert_eq!(mode, SandboxNetworkMode::ParentNetns);
}

// Given: NetworkAccess::Allowed (ロール定義レベルで常に許可)
// When: explicit_opt_in = true でマッピングする
// Then: ParentNetns に解決される
#[test]
fn allowed_with_explicit_opt_in_maps_to_parent_netns() {
    let mode = sandbox_network_mode(NetworkAccess::Allowed, true);

    assert_eq!(mode, SandboxNetworkMode::ParentNetns);
}

// Given: NetworkAccess::OptIn (明示的オプトイン時のみ許可)
// When: explicit_opt_in = false でマッピングする
// Then: Unshared に解決される
#[test]
fn opt_in_without_explicit_opt_in_maps_to_unshared() {
    let mode = sandbox_network_mode(NetworkAccess::OptIn, false);

    assert_eq!(mode, SandboxNetworkMode::Unshared);
}

// Given: NetworkAccess::OptIn (明示的オプトイン時のみ許可)
// When: explicit_opt_in = true でマッピングする
// Then: ParentNetns に解決される
#[test]
fn opt_in_with_explicit_opt_in_maps_to_parent_netns() {
    let mode = sandbox_network_mode(NetworkAccess::OptIn, true);

    assert_eq!(mode, SandboxNetworkMode::ParentNetns);
}

// Given: NetworkAccess::Denied (ADR 0008 default-deny)
// When: explicit_opt_in = false でマッピングする
// Then: Unshared に解決される
#[test]
fn denied_without_explicit_opt_in_maps_to_unshared() {
    let mode = sandbox_network_mode(NetworkAccess::Denied, false);

    assert_eq!(mode, SandboxNetworkMode::Unshared);
}

// Given: NetworkAccess::Denied (ADR 0008 default-deny)
// When: explicit_opt_in = true でマッピングする
// Then: オプトインがあっても Unshared のまま解決される
#[test]
fn denied_with_explicit_opt_in_maps_to_unshared() {
    let mode = sandbox_network_mode(NetworkAccess::Denied, true);

    assert_eq!(mode, SandboxNetworkMode::Unshared);
}

// Given: ネットワーク要件が Denied の 2 ロール (Worker / Reviewer) のポリシー
// When: sandbox_network_mode を呼ぶ
// Then: すべて Unshared に解決される
#[test]
fn for_role_denied_roles_map_to_unshared() {
    for role in [Role::Worker, Role::Reviewer] {
        let policy = ExecutionPolicy::for_role(role);

        assert_eq!(
            policy.sandbox_network_mode(),
            SandboxNetworkMode::Unshared,
            "{} は Unshared に解決されるべき",
            role.name()
        );
    }
}

// Given: ネットワーク要件が OptIn の 2 ロール (Explorer / Orchestrator) のポリシー
// When: sandbox_network_mode を呼ぶ (v0.1 にはオプトイン経路がない)
// Then: fail-closed によりすべて Unshared に解決される
#[test]
fn for_role_opt_in_roles_map_to_unshared_fail_closed() {
    for role in [Role::Explorer, Role::Orchestrator] {
        let policy = ExecutionPolicy::for_role(role);

        assert_eq!(
            policy.sandbox_network_mode(),
            SandboxNetworkMode::Unshared,
            "{} は fail-closed で Unshared に解決されるべき",
            role.name()
        );
    }
}

// Given: role_name は Worker だがケイパビリティの network が Allowed の手組みポリシー
// When: sandbox_network_mode を呼ぶ
// Then: ロール名ではなくケイパビリティ境界に従い ParentNetns に解決される
#[test]
fn hand_built_policy_with_allowed_network_maps_to_parent_netns() {
    let policy = ExecutionPolicy {
        capabilities: RoleCapabilities::new(["read"], NetworkAccess::Allowed, false),
        role_name: "Worker".to_string(),
    };

    assert_eq!(
        policy.sandbox_network_mode(),
        SandboxNetworkMode::ParentNetns
    );
}

// Given: 親名前空間のローカル TCP 待受と deny/allow 両方のポリシー
// When: build_sandbox で構築したサンドボックス内から bash で接続する
// Then: deny の Worker は --unshare-net で接続に失敗し allow の手組みポリシーは親 netns で成功する
#[ignore = "bwrap 実行環境が必要"]
#[test]
fn denied_role_cannot_connect_but_allowed_policy_can() {
    let workspace = tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("作業領域を作成できるはずです");
    let listener = TcpListener::bind("127.0.0.1:0").expect("ローカル待受を作成できるはずです");
    let address = listener
        .local_addr()
        .expect("待受アドレスを取得できるはずです");
    TcpStream::connect(address).expect("親名前空間から接続できるはずです");
    let script = format!("echo hi > /dev/tcp/127.0.0.1/{}", address.port());

    // Given: Worker のポリシー (NetworkAccess::Denied)
    let policy = ExecutionPolicy::for_role(Role::Worker);
    // When: build_sandbox でサンドボックスを構築して bash の接続コマンドを包む
    let sandbox =
        build_sandbox(&policy, workspace.path().to_path_buf()).expect("bwrap 実行環境が必要です");
    let wrapped = sandbox
        .wrap(CommandSpec {
            program: "bash".to_owned(),
            args: vec!["-c".to_owned(), script.clone()],
            cwd: Some(workspace.path().to_path_buf()),
            extra_env: Vec::new(),
        })
        .expect("コマンドを包めるはずです");
    // Then: argv は --unshare-net を含み接続は失敗する
    assert!(
        wrapped.args.iter().any(|arg| arg == "--unshare-net"),
        "deny ポリシーの argv は --unshare-net を含むはずです"
    );
    assert!(
        !run(wrapped).status.success(),
        "deny ポリシーからの接続は失敗するはずです"
    );

    // Given: role_name は Worker だが network が Allowed の手組みポリシー
    let policy = ExecutionPolicy {
        capabilities: RoleCapabilities::new(["read"], NetworkAccess::Allowed, false),
        role_name: "Worker".to_string(),
    };
    // When: build_sandbox でサンドボックスを構築して同じ bash の接続コマンドを包む
    let sandbox =
        build_sandbox(&policy, workspace.path().to_path_buf()).expect("bwrap 実行環境が必要です");
    let wrapped = sandbox
        .wrap(CommandSpec {
            program: "bash".to_owned(),
            args: vec!["-c".to_owned(), script],
            cwd: Some(workspace.path().to_path_buf()),
            extra_env: Vec::new(),
        })
        .expect("コマンドを包めるはずです");
    // Then: argv は --unshare-net を含まず同一待受への接続は成功する
    assert!(
        !wrapped.args.iter().any(|arg| arg == "--unshare-net"),
        "allow ポリシーの argv は --unshare-net を含まないはずです"
    );
    let output = run(wrapped);
    assert!(
        output.status.success(),
        "allow ポリシーからの接続は成功するはずです: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
