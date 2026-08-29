//! usage 集計を担うモジュールです。

use std::{collections::HashMap, time::UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::event::{EventMeta, UsageEvent};

/// 1 分バケットの識別子。
///
/// `window_start` は壁時計の epoch 秒を分で切り捨てた値
/// (`epoch_secs / 60 * 60`)。壁時計基準にすることでプロセス再起動を跨いで意味を保つ。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BucketKey {
    pub window_start: u64,
    pub provider: String,
    pub model: String,
}

/// 1 分バケットあたりの集計値（ADR 0012 の downsampled 単位）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageBucket {
    pub key: BucketKey,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// `Usage` バリアントの観測回数（`CacheStats` は含めない）。
    pub request_count: u64,
}

/// 分単位の usage バケットを蓄積する集計器。
#[derive(Default)]
pub struct UsageAggregator {
    buckets: HashMap<BucketKey, UsageBucket>,
}

impl UsageAggregator {
    /// 空の集計器を生成する。
    pub fn new() -> Self {
        Self::default()
    }

    /// usage イベントを該当バケットへ集約する。window は `meta.wall_clock` から導出する。
    pub fn record(&mut self, usage: &UsageEvent, meta: &EventMeta) {
        let window_start = meta
            .wall_clock
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() / 60 * 60)
            .unwrap_or(0);
        let (provider, model) = match usage {
            UsageEvent::Usage {
                provider, model, ..
            }
            | UsageEvent::CacheStats {
                provider, model, ..
            } => (provider, model),
        };
        let key = BucketKey {
            window_start,
            provider: provider.clone(),
            model: model.clone(),
        };
        let bucket = self
            .buckets
            .entry(key)
            .or_insert_with_key(|key| UsageBucket {
                key: key.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_hits: 0,
                cache_misses: 0,
                request_count: 0,
            });

        match usage {
            UsageEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                ..
            } => {
                bucket.input_tokens += input_tokens;
                bucket.output_tokens += output_tokens;
                bucket.cache_read_tokens += cache_read_tokens;
                bucket.cache_write_tokens += cache_write_tokens;
                bucket.request_count += 1;
            }
            UsageEvent::CacheStats {
                cache_hits,
                cache_misses,
                ..
            } => {
                bucket.cache_hits += cache_hits;
                bucket.cache_misses += cache_misses;
            }
        }
    }

    /// 全バケットを `(window_start, provider, model)` 昇順で取り出し、内部を空にする。
    pub fn drain(&mut self) -> Vec<UsageBucket> {
        let mut buckets = std::mem::take(&mut self.buckets)
            .into_values()
            .collect::<Vec<_>>();
        buckets.sort_by(|left, right| {
            (left.key.window_start, &left.key.provider, &left.key.model).cmp(&(
                right.key.window_start,
                &right.key.provider,
                &right.key.model,
            ))
        });
        buckets
    }

    /// drain したバケットを sink へ渡す。
    pub fn flush_into(&mut self, sink: &dyn UsageSink) {
        sink.submit(self.drain());
    }
}

/// storage の single-writer へ downsampled バケットを渡す土台（ADR 0012）。
///
/// v0.1 は同期 trait。将来の非同期 writer 導入時に async trait 化する余地を残す。
pub trait UsageSink {
    fn submit(&self, buckets: Vec<UsageBucket>);
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        time::{Duration, UNIX_EPOCH},
    };

    use crate::{
        event::{EventMeta, UsageEvent},
        usage::{BucketKey, UsageAggregator, UsageBucket, UsageSink},
    };

    struct MockSink(RefCell<Vec<UsageBucket>>);

    impl UsageSink for MockSink {
        fn submit(&self, buckets: Vec<UsageBucket>) {
            self.0.borrow_mut().extend(buckets);
        }
    }

    fn meta_at(epoch_seconds: u64) -> EventMeta {
        EventMeta {
            schema_version: 1,
            monotonic: Duration::ZERO,
            wall_clock: UNIX_EPOCH + Duration::from_secs(epoch_seconds),
        }
    }

    fn usage_event(provider: &str, model: &str, input_tokens: u64) -> UsageEvent {
        UsageEvent::Usage {
            provider: provider.into(),
            model: model.into(),
            input_tokens,
            output_tokens: input_tokens + 1,
            cache_read_tokens: input_tokens + 2,
            cache_write_tokens: input_tokens + 3,
        }
    }

    #[test]
    fn record_sums_usage_events_in_the_same_minute() {
        // Given: 同一プロバイダ・モデル・分に属する二つの usage イベント。
        let mut aggregator = UsageAggregator::new();
        let meta = meta_at(119);

        // When: 両方のイベントを記録する。
        aggregator.record(&usage_event("anthropic", "model-a", 10), &meta);
        aggregator.record(&usage_event("anthropic", "model-a", 20), &meta);

        // Then: 一つのバケットに各トークン値とリクエスト数が合算される。
        assert_eq!(
            aggregator.drain(),
            vec![UsageBucket {
                key: BucketKey {
                    window_start: 60,
                    provider: "anthropic".into(),
                    model: "model-a".into(),
                },
                input_tokens: 30,
                output_tokens: 32,
                cache_read_tokens: 34,
                cache_write_tokens: 36,
                cache_hits: 0,
                cache_misses: 0,
                request_count: 2,
            }]
        );
    }

    #[test]
    fn record_separates_buckets_by_provider_model_and_minute() {
        // Given: provider、model、分がそれぞれ異なる usage イベント。
        let mut aggregator = UsageAggregator::new();

        // When: 各イベントを記録する。
        aggregator.record(&usage_event("alpha", "model-a", 1), &meta_at(1));
        aggregator.record(&usage_event("beta", "model-a", 1), &meta_at(1));
        aggregator.record(&usage_event("alpha", "model-b", 1), &meta_at(1));
        aggregator.record(&usage_event("alpha", "model-a", 1), &meta_at(60));

        // Then: 各識別子の組み合わせが別バケットになる。
        let buckets = aggregator.drain();
        assert_eq!(buckets.len(), 4);
        assert_eq!(
            buckets
                .iter()
                .map(|bucket| {
                    (
                        bucket.key.window_start,
                        bucket.key.provider.as_str(),
                        bucket.key.model.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (0, "alpha", "model-a"),
                (0, "alpha", "model-b"),
                (0, "beta", "model-a"),
                (60, "alpha", "model-a"),
            ]
        );
    }

    #[test]
    fn record_merges_cache_stats_without_incrementing_request_count() {
        // Given: 同じバケットの usage と cache statistics イベント。
        let mut aggregator = UsageAggregator::new();
        let meta = meta_at(60);

        // When: 両イベントを記録する。
        aggregator.record(&usage_event("anthropic", "model-a", 5), &meta);
        aggregator.record(
            &UsageEvent::CacheStats {
                provider: "anthropic".into(),
                model: "model-a".into(),
                cache_hits: 7,
                cache_misses: 3,
            },
            &meta,
        );

        // Then: cache 値のみが追加され、リクエスト数は usage の回数に留まる。
        let bucket = aggregator.drain().pop().expect("one bucket");
        assert_eq!((bucket.cache_hits, bucket.cache_misses), (7, 3));
        assert_eq!(bucket.request_count, 1);
    }

    #[test]
    fn drain_sorts_buckets_and_empties_the_aggregator() {
        // Given: 順不同で記録された複数バケット。
        let mut aggregator = UsageAggregator::new();
        aggregator.record(&usage_event("zeta", "model-z", 1), &meta_at(120));
        aggregator.record(&usage_event("beta", "model-b", 1), &meta_at(60));
        aggregator.record(&usage_event("alpha", "model-a", 1), &meta_at(60));

        // When: バケットを drain する。
        let buckets = aggregator.drain();

        // Then: キー昇順で得られ、次の drain は空になる。
        assert_eq!(
            buckets
                .iter()
                .map(|bucket| {
                    (
                        bucket.key.window_start,
                        bucket.key.provider.as_str(),
                        bucket.key.model.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (60, "alpha", "model-a"),
                (60, "beta", "model-b"),
                (120, "zeta", "model-z"),
            ]
        );
        assert!(aggregator.drain().is_empty());
    }

    #[test]
    fn flush_into_submits_exactly_the_drained_buckets() {
        // Given: 一つの集計済みバケットと同期 sink。
        let mut aggregator = UsageAggregator::new();
        aggregator.record(&usage_event("anthropic", "model-a", 3), &meta_at(0));
        let sink = MockSink(RefCell::new(Vec::new()));

        // When: sink へ flush する。
        aggregator.flush_into(&sink);

        // Then: drain 結果だけが sink へ渡され、アグリゲータは空になる。
        assert_eq!(
            sink.0.borrow().as_slice(),
            [UsageBucket {
                key: BucketKey {
                    window_start: 0,
                    provider: "anthropic".into(),
                    model: "model-a".into(),
                },
                input_tokens: 3,
                output_tokens: 4,
                cache_read_tokens: 5,
                cache_write_tokens: 6,
                cache_hits: 0,
                cache_misses: 0,
                request_count: 1,
            }]
        );
        assert!(aggregator.drain().is_empty());
    }

    #[test]
    fn record_aligns_window_start_to_the_minute_boundary() {
        // Given: 119 秒と 120 秒の壁時計時刻。
        let mut aggregator = UsageAggregator::new();

        // When: それぞれを記録する。
        aggregator.record(&usage_event("anthropic", "model-a", 1), &meta_at(119));
        aggregator.record(&usage_event("anthropic", "model-a", 1), &meta_at(120));

        // Then: window start は 60 秒境界へ切り捨てられる。
        assert_eq!(
            aggregator
                .drain()
                .into_iter()
                .map(|bucket| bucket.key.window_start)
                .collect::<Vec<_>>(),
            vec![60, 120]
        );
    }
}
