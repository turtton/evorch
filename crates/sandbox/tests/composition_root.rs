//! 本番用コンポジションルートによる実 bwrap での構築を検証します。

use sandbox::{BwrapConfig, CommandSpec, production_sandbox};

// Given: 実行環境で bwrap が利用できる / When: 本番用コンポジションルートで構築する / Then: コマンドを bwrap で包める
#[ignore = "bwrap 実行環境が必要"]
#[test]
fn production_sandbox_constructs_with_real_bwrap() {
    let workspace = std::env::current_dir().expect("作業領域を取得できるはずです");
    let sandbox = production_sandbox(BwrapConfig::new(workspace))
        .expect("本番用サンドボックスを構築できるはずです");
    let wrapped = sandbox
        .wrap(CommandSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            cwd: None,
            extra_env: Vec::new(),
        })
        .expect("コマンドを包めるはずです");

    assert!(
        wrapped.program.ends_with("bwrap"),
        "ラップ結果のプログラムは bwrap のはずです: {}",
        wrapped.program
    );
}
