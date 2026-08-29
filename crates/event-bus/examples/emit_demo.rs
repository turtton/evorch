use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use event_bus::bus::{EventBus, RecvError};
use event_bus::event::{
    Event, FaultEvent, LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent, UsageEvent,
};
use event_bus::usage::{UsageAggregator, UsageBucket, UsageSink};
use tokio::task::JoinSet;

struct PrintSink {
    count: Arc<AtomicUsize>,
}

impl UsageSink for PrintSink {
    fn submit(&self, buckets: Vec<UsageBucket>) {
        self.count.fetch_add(buckets.len(), Ordering::Relaxed);
        for bucket in buckets {
            match serde_json::to_string(&bucket) {
                Ok(json) => println!("[usage-bucket] {json}"),
                Err(error) => eprintln!("[usage-bucket] serialization failed: {error}"),
            }
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let bus = Arc::new(EventBus::new(8));
    let mut subscribers = JoinSet::new();
    for (name, delay) in [
        ("fast", Duration::ZERO),
        ("slow", Duration::from_millis(30)),
    ] {
        let mut receiver = bus.subscribe();
        subscribers.spawn(async move {
            // ストリーム末尾の fault イベントまで到達できるよう、emit 総数
            // （27 件）+ lag 通知分に余裕を持たせる。
            for _ in 0..60 {
                match tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
                    Ok(Ok(event)) => match serde_json::to_string(&event) {
                        Ok(json) => println!("[{name}] {json}"),
                        Err(error) => eprintln!("[{name}] serialization failed: {error}"),
                    },
                    Ok(Err(RecvError::Lagged(skipped))) => {
                        println!("[{name}] lagged: n={skipped}");
                    }
                    Ok(Err(RecvError::Closed)) | Err(_) => break,
                }
                tokio::time::sleep(delay).await;
            }
        });
    }

    tokio::task::yield_now().await;
    // 各 emit の後で yield し、購読者が追従できるようにする。
    // 12 連続バーストは emit 間に 1ms sleep を挟み、スケジューラに実行枠を渡す。
    // fast 購読者は追従し、1 イベントあたり 30ms かかる slow 購読者だけが
    // 意図的に lag する。
    macro_rules! emit {
        ($kind:expr) => {{
            bus.emit(Event::new($kind));
            tokio::task::yield_now().await;
        }};
    }
    emit!(LifecycleEvent::Started {
        session_id: "session-demo-001".into(),
    });
    emit!(MessageEvent::MessageDelta {
        delta: "Hello, event bus".into(),
    });
    emit!(MessageEvent::ReasoningDelta {
        delta: "selecting a tool".into(),
    });
    emit!(ToolEvent::ToolStarted {
        tool_name: "search_docs".into(),
        call_id: "call-42".into(),
    });
    emit!(ToolEvent::ToolCompleted {
        tool_name: "search_docs".into(),
        call_id: "call-42".into(),
        is_error: false,
    });
    for index in 0..12 {
        bus.emit(Event::new(MessageEvent::MessageDelta {
            delta: format!(" burst-{index}"),
        }));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    emit!(LifecycleEvent::Delegated {
        session_id: "session-demo-001".into(),
        target: "worker-search".into(),
    });
    emit!(LifecycleEvent::BackgroundTaskStarted {
        task_id: "task-99".into(),
    });
    emit!(LifecycleEvent::BackgroundTaskCompleted {
        task_id: "task-99".into(),
    });
    emit!(ProviderEvent::ProviderFallback {
        from_provider: "anthropic".into(),
        to_provider: "openai".into(),
        reason: "upstream timeout".into(),
    });
    emit!(LifecycleEvent::Completed {
        session_id: "session-demo-001".into(),
    });

    let usage_events = [
        Event::new(UsageEvent::Usage {
            provider: "anthropic".into(),
            model: "claude-sonnet-4".into(),
            input_tokens: 1_240,
            output_tokens: 386,
            cache_read_tokens: 900,
            cache_write_tokens: 120,
        }),
        Event::new(UsageEvent::Usage {
            provider: "openai".into(),
            model: "gpt-5".into(),
            input_tokens: 820,
            output_tokens: 214,
            cache_read_tokens: 400,
            cache_write_tokens: 80,
        }),
        Event::new(UsageEvent::CacheStats {
            provider: "anthropic".into(),
            model: "claude-sonnet-4".into(),
            cache_hits: 18,
            cache_misses: 3,
        }),
    ];
    let mut aggregator = UsageAggregator::new();
    for event in &usage_events {
        let event_bus::event::EventKind::Usage(usage) = &event.kind else {
            continue;
        };
        aggregator.record(usage, &event.meta);
        bus.emit(event.clone());
        tokio::task::yield_now().await;
    }
    emit!(LifecycleEvent::Failed {
        session_id: "session-demo-001".into(),
        reason: "demonstration failure path".into(),
    });
    bus.emit(Event::new(FaultEvent::SubscriberLagged {
        subscriber_id: 999,
        skipped: 2,
    }));

    let count = Arc::new(AtomicUsize::new(0));
    aggregator.flush_into(&PrintSink {
        count: Arc::clone(&count),
    });
    println!("drained bucket count: {}", count.load(Ordering::Relaxed));
    while subscribers.join_next().await.is_some() {}
}
