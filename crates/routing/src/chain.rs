//! ロールのチェーンの展開と、一周するフォールバックの走査 (ADR 0030)。

use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use crate::{ResolvedRoute, Router};

/// 展開したチェーンの 1 メンバー (route の候補 1 件)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainMember {
    /// 属するチェーン要素の位置。同じ値のメンバーが 1 つのプールを成す。
    pub entry: usize,
    /// 属する route (経路プール) の名前。
    pub route: String,
    /// 送り先のプロファイルと具体モデル。
    pub target: ResolvedRoute,
    /// チェーン要素で指定した推論強度。
    pub reasoning_effort: Option<String>,
}

/// route の候補を宣言順に連結したチェーン。プールの境界は [`ChainMember::entry`] で表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    key: String,
    members: Vec<ChainMember>,
}

impl Chain {
    pub(crate) fn new(key: String, members: Vec<ChainMember>) -> Self {
        Self { key, members }
    }

    /// アフィニティでピンを引くキー (ロールとカテゴリ)。
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 展開したメンバー。
    pub fn members(&self) -> &[ChainMember] {
        &self.members
    }

    /// `position` のメンバー。
    ///
    /// # Panics
    /// `position` が範囲外のとき。
    pub fn member(&self, position: usize) -> &ChainMember {
        &self.members[position]
    }

    /// (プロファイル, 具体モデル) が一致する最初のメンバーの位置。
    pub fn position_of(&self, profile: &str, model: &str) -> Option<usize> {
        self.members
            .iter()
            .position(|member| member.target.profile == profile && member.target.model_id == model)
    }

    /// `origin` の次から試す順序。`origin` 自身は含まない。
    ///
    /// まず `origin` と同じプールの残り (後ろ → 前の順に一周)、次に後続のプール、
    /// 最後に先頭へ戻って前のプールを宣言順に並べる。プールの全メンバーを試すまで
    /// 境界を越えないため、同じモデルの経路を使い切ってから別モデルへ移る。
    pub(crate) fn scan_order(&self, origin: usize) -> Vec<usize> {
        let pool = self.members[origin].entry;
        let in_pool = |position: &usize| self.members[*position].entry == pool;
        let after = origin + 1..self.members.len();
        let before = 0..origin;
        after
            .clone()
            .filter(in_pool)
            .chain(before.clone().filter(in_pool))
            .chain(after.filter(|position| !in_pool(position)))
            .chain(before.filter(|position| !in_pool(position)))
            .collect()
    }
}

/// [`FallbackCycle::next`] が示す次の動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackStep {
    /// このメンバーへ送る。
    Try(usize),
    /// 指定時間待ってから、もう一度 [`FallbackCycle::next`] を呼ぶ。
    Wait(Duration),
    /// 一周を終えた。エラーとして返す。
    Exhausted,
}

#[derive(Debug, Clone, Copy)]
struct Failure {
    transient: bool,
    retry_after: Option<Duration>,
}

/// 1 リクエスト分のフォールバックの一周。
///
/// - 失敗した位置 (`origin`) の次から [`Chain::scan_order`] の順に、各メンバーを一度だけ試す。
///   クールダウン中や利用できないメンバーは送らずに飛ばす。
/// - `origin` の失敗が一時的なものだった場合だけ、一周の最後に `origin` をもう一度試す。
/// - プールを離れるとき、そのプールの全メンバーが失敗済みか利用不可で、最後の失敗が
///   一時的かつ Retry-After が上限以下なら、一度だけ待ってそのプールの一時的に
///   失敗したメンバーを再試行する。待機後の再試行はクールダウンを見ない
///   (待った時間がそのクールダウンである)。
#[derive(Debug)]
pub struct FallbackCycle {
    origin: usize,
    order: VecDeque<usize>,
    tried: BTreeSet<(String, String)>,
    failures: Vec<Option<Failure>>,
    last_failed: Option<usize>,
    retries: VecDeque<usize>,
    origin_recorded: bool,
    waited: bool,
}

impl FallbackCycle {
    /// `origin` で失敗したあとの一周を始める。続けて `origin` の失敗を
    /// [`FallbackCycle::record_failure`] で記録する。
    pub fn new(chain: &Chain, origin: usize) -> Self {
        let mut tried = BTreeSet::new();
        tried.insert(member_key(chain, origin));
        Self {
            origin,
            order: chain.scan_order(origin).into(),
            tried,
            failures: vec![None; chain.members.len()],
            last_failed: None,
            retries: VecDeque::new(),
            origin_recorded: false,
            waited: false,
        }
    }

    /// 送ったメンバーの失敗を記録する。
    pub fn record_failure(
        &mut self,
        position: usize,
        transient: bool,
        retry_after: Option<Duration>,
    ) {
        self.failures[position] = Some(Failure {
            transient,
            retry_after,
        });
        self.last_failed = Some(position);
        if position == self.origin && !self.origin_recorded {
            self.origin_recorded = true;
            if transient {
                self.order.push_back(self.origin);
            }
        }
    }

    /// 次に送るメンバー、待機、または打ち止めを返す。
    pub fn next(&mut self, router: &Router, chain: &Chain, max_wait: Duration) -> FallbackStep {
        if let Some(position) = self.retries.pop_front() {
            return FallbackStep::Try(position);
        }
        while let Some(&position) = self.order.front() {
            if let Some(wait) = self.wait_before(router, chain, Some(position), max_wait) {
                return FallbackStep::Wait(wait);
            }
            self.order.pop_front();
            let key = member_key(chain, position);
            let retrying_origin = position == self.origin;
            if (!retrying_origin && self.tried.contains(&key))
                || !router.is_member_available(&chain.members[position].target)
            {
                continue;
            }
            self.tried.insert(key);
            return FallbackStep::Try(position);
        }
        match self.wait_before(router, chain, None, max_wait) {
            Some(wait) => FallbackStep::Wait(wait),
            None => FallbackStep::Exhausted,
        }
    }

    /// `next` へ進む前に、離れるプールの Retry-After を待つべきなら待機時間を返す。
    fn wait_before(
        &mut self,
        router: &Router,
        chain: &Chain,
        next: Option<usize>,
        max_wait: Duration,
    ) -> Option<Duration> {
        if self.waited {
            return None;
        }
        let last = self.last_failed?;
        let pool = chain.members[last].entry;
        if next.is_some_and(|next| chain.members[next].entry == pool) {
            return None;
        }
        let exhausted = chain
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| member.entry == pool)
            .all(|(position, member)| {
                self.tried.contains(&member_key(chain, position))
                    || !router.is_member_available(&member.target)
            });
        let failure = self.failures[last]?;
        let wait = failure.retry_after.filter(|wait| *wait <= max_wait)?;
        if !exhausted || !failure.transient || wait.is_zero() {
            return None;
        }
        self.waited = true;
        self.retries = (0..chain.members.len())
            .filter(|position| {
                chain.members[*position].entry == pool
                    && self.failures[*position].is_some_and(|failure| failure.transient)
            })
            .collect();
        if self.retries.contains(&self.origin) {
            self.order.retain(|position| *position != self.origin);
        }
        Some(wait)
    }
}

fn member_key(chain: &Chain, position: usize) -> (String, String) {
    let target = &chain.members[position].target;
    (target.profile.clone(), target.model_id.clone())
}

#[cfg(test)]
mod tests;
