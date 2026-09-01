//! web_search の provider API key main process 隔離テスト (AC4)。
//!
//! 環境変数を変更するため、テストごとに独立プロセスで走る専用 integration test
//! binary とする。このバイナリ内のテストは 1 件のみである。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use sandbox::{CommandSpec, DirectSandbox, Sandbox};
use serde_json::json;
use tools::{SearchError, SearchOptions, SearchProvider, SearchResults, Tool, WebSearch};

/// 呼び出し回数を数える stub provider。
struct StubSearchProvider {
    name: &'static str,
    result: Result<SearchResults, SearchError>,
    calls: AtomicU32,
}

impl StubSearchProvider {
    fn new(name: &'static str, result: Result<SearchResults, SearchError>) -> Self {
        Self {
            name,
            result,
            calls: AtomicU32::new(0),
        }
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SearchProvider for StubSearchProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn search(
        &self,
        _query: &str,
        _options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

fn exa_ok() -> Result<SearchResults, SearchError> {
    Ok(SearchResults {
        content: "Title: Exa result\nURL: https://example.com/exa".to_owned(),
        result_count: 1,
        request_id: Some("req-exa-1".to_owned()),
        usage: None,
    })
}

// Given: EXA_API_KEY / TAVILY_API_KEY が設定された main process 環境 / When: web_search を実行し sandbox でコマンドを包む / Then: 実行器は key_present_unused を報告し、子コマンドの env には両キーが現れない (AC4)
#[tokio::test]
async fn provider_api_keys_stay_in_main_process_and_out_of_sandbox_child_env() {
    // SAFETY: この integration-test binary 内で環境変数を変更するのはこの単一の test fn のみであり、変更後に並行して環境変数を読む別スレッドは存在しない。
    unsafe {
        std::env::set_var("EXA_API_KEY", "k");
        std::env::set_var("TAVILY_API_KEY", "k");
    }

    // Assert A: main process 側の消費。実環境 lookup で key の存在を検出し、
    // 現行 transport では未使用であることを credential_status で報告する。
    let primary_stub = Arc::new(StubSearchProvider::new("exa", exa_ok()));
    let fallback_stub = Arc::new(StubSearchProvider::new(
        "tavily",
        Err(SearchError::HttpStatus(429)),
    ));
    let primary: Arc<dyn SearchProvider> = primary_stub.clone();
    let fallback: Arc<dyn SearchProvider> = fallback_stub.clone();
    let tool = WebSearch::for_providers(primary, fallback);

    let result = Tool::execute(&tool, json!({ "query": "evorch" }))
        .await
        .expect("検索は成功するはずです");

    assert!(!result.is_error);
    let detail = result.detail.expect("成功時は metadata が付く");
    assert_eq!(
        detail["credential_status"], "key_present_unused",
        "key 存在下の credential_status が不正: {detail}"
    );
    assert_eq!(primary_stub.calls(), 1);
    assert_eq!(fallback_stub.calls(), 0);
    assert!(
        !detail.to_string().contains("\"k\""),
        "metadata に key 値が漏れていない: {detail}"
    );

    // Assert B: sandbox child env への非露出。merge_environment の許可リスト
    // (PATH / TERM / LANG / LC_ALL) は DirectSandbox::wrap と BwrapSandbox::wrap
    // で共有されるため、Direct 経路の証明で許可リストの首根っこを検証する。
    let wrapped = DirectSandbox::new_unchecked()
        .wrap(CommandSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            cwd: None,
            extra_env: Vec::new(),
        })
        .expect("コマンドを包めるはずです");

    assert!(
        !wrapped
            .env
            .iter()
            .any(|(key, _)| key == "EXA_API_KEY" || key == "TAVILY_API_KEY"),
        "子コマンド env に provider API key が含まれる: {:?}",
        wrapped.env
    );
    assert_eq!(
        std::env::var("EXA_API_KEY").as_deref(),
        Ok("k"),
        "main process 側には key が存在する"
    );
    assert_eq!(std::env::var("TAVILY_API_KEY").as_deref(), Ok("k"));
}
