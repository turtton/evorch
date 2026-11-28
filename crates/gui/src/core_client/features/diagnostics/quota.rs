//! Quota state and footer rendering, isolated from Live/scoped usage (E4 PR3).
use super::{FeatureContext, FeatureReply, Pending, Visibility};
use crate::theme::{
    text::muted,
    tokens::{FONT_SMALL, palette},
};
use std::time::{Duration, Instant};
use workbench_protocol::{
    events::UiEvent,
    namespaces::diagnostics::{self as wire, DiagnosticsEvent, DiagnosticsQuery, DiagnosticsReply},
    queries::{Query, QueryReply},
};

const RETRY: Duration = Duration::from_secs(30);

pub struct Quotas {
    pub(super) snapshot: wire::ProviderQuotas,
    dirty: bool,
    in_flight: bool,
    next: Option<Instant>,
}
impl Default for Quotas {
    fn default() -> Self {
        Self {
            snapshot: wire::ProviderQuotas::default(),
            dirty: true,
            in_flight: false,
            next: None,
        }
    }
}
impl Quotas {
    pub(super) fn maintain(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        visibility: Visibility,
        next_tag: &mut u64,
        pending: &mut std::collections::BTreeMap<u64, Pending>,
    ) {
        if !visibility.available || !self.take_due(Instant::now()) {
            return;
        }
        *next_tag += 1;
        pending.insert(*next_tag, Pending::Quota);
        ctx.query(
            *next_tag,
            Query::Diagnostics(DiagnosticsQuery::GetProviderQuotas(
                wire::GetProviderQuotas {},
            )),
        );
    }
    fn take_due(&mut self, now: Instant) -> bool {
        if !self.dirty || self.in_flight || self.next.is_some_and(|next| now < next) {
            return false;
        }
        self.dirty = false;
        self.in_flight = true;
        self.next = None;
        true
    }
    pub(super) fn event(&mut self, event: &UiEvent) {
        if matches!(
            event,
            UiEvent::Diagnostics(DiagnosticsEvent::ProviderQuotasChanged {})
        ) {
            self.dirty = true;
        }
    }
    pub(super) fn reply(&mut self, reply: FeatureReply) {
        self.in_flight = false;
        if let FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::ProviderQuotas(
            snapshot,
        )))) = reply
        {
            self.snapshot = snapshot;
        } else {
            // Do not surface a transport/server error's arbitrary text as a tooltip.
            self.dirty = true;
            self.next = Some(Instant::now() + RETRY);
        }
    }
    pub fn refresh_in(&self, now: Instant) -> Option<Duration> {
        (self.dirty && !self.in_flight).then(|| {
            self.next
                .map_or(Duration::ZERO, |next| next.saturating_duration_since(now))
        })
    }
    pub fn profiles(&self) -> &[wire::ProviderQuota] {
        &self.snapshot.profiles
    }
}

pub fn footer(ui: &mut egui::Ui, profiles: &[wire::ProviderQuota]) {
    for quota in profiles {
        let warning = matches!(
            quota.state,
            wire::QuotaState::Stale | wire::QuotaState::Unavailable
        );
        let values = quota
            .windows
            .iter()
            .map(|w| {
                format!(
                    "{} {}",
                    w.label,
                    w.remaining_basis_points
                        .map_or_else(|| "—".into(), |p| format!("{:.0}%", f64::from(p) / 100.0))
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let value = if values.is_empty() { "—" } else { &values };
        let label = format!(
            "{} {value}{}",
            quota.profile,
            if warning { " ⚠" } else { "" }
        );
        let text = if warning {
            egui::RichText::new(&label).color(palette().WARNING)
        } else {
            muted(&label)
        };
        let response = ui.add(egui::Label::new(text.size(FONT_SMALL)).sense(egui::Sense::hover()));
        crate::theme::widgets::accessible(&response, &format!("Quota {}", quota.profile));
        response.on_hover_text(tooltip(quota));
    }
}
fn tooltip(quota: &wire::ProviderQuota) -> String {
    let mut text = format!("{} quota", quota.profile);
    if quota.state == wire::QuotaState::Loading {
        text.push_str("\nLoading automatically…");
    }
    if quota.state == wire::QuotaState::Stale {
        text.push_str("\nLast successful value (stale)");
    }
    if let Some(issue) = quota.issue {
        text.push('\n');
        text.push_str(issue_text(issue.code));
    }
    for w in &quota.windows {
        text.push_str(&format!(
            "\n{}: {} remaining",
            w.label,
            w.remaining_basis_points.map_or_else(
                || "unknown".into(),
                |p| format!("{:.1}%", f64::from(p) / 100.0)
            )
        ));
        if let Some(usage) = &w.usage_label {
            text.push_str(&format!(" ({usage})"));
        }
        if let Some(at) = w
            .resets_at_ms
            .and_then(|ms| i64::try_from(ms).ok())
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            text.push_str(&format!(
                "; resets {}",
                at.with_timezone(&chrono::Local).format("%m-%d %H:%M")
            ));
        }
    }
    text
}
fn issue_text(code: wire::QuotaIssueCode) -> &'static str {
    use wire::QuotaIssueCode::*;
    match code {
        Credentials => "Quota credentials are unavailable.",
        ReauthenticationRequired => "Sign in again to read quota.",
        Timeout => "Quota request timed out; retrying automatically.",
        Network => "Quota service could not be reached; retrying automatically.",
        Http => "Quota service rejected the request; retrying automatically.",
        Protocol => "Quota service returned an unsupported response.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_during_a_query_coalesce_and_off_replaces_the_list() {
        let mut quotas = Quotas::default();
        let now = Instant::now();
        assert!(quotas.take_due(now));
        for _ in 0..8 {
            quotas.event(&UiEvent::Diagnostics(
                DiagnosticsEvent::ProviderQuotasChanged {},
            ));
        }
        assert!(!quotas.take_due(now + Duration::from_secs(1)));
        quotas.reply(FeatureReply::Query(Ok(QueryReply::Diagnostics(
            DiagnosticsReply::ProviderQuotas(wire::ProviderQuotas::default()),
        ))));
        assert!(quotas.take_due(now + Duration::from_secs(1)));
        quotas.reply(FeatureReply::Query(Ok(QueryReply::Diagnostics(
            DiagnosticsReply::ProviderQuotas(wire::ProviderQuotas::default()),
        ))));
        assert!(quotas.profiles().is_empty());
        assert!(!quotas.take_due(now + RETRY));
    }
    #[test]
    fn footer_chips_tooltip_and_off_list_are_drawn_from_core_snapshot() {
        use egui_kittest::{Harness, kittest::Queryable};
        let profiles = vec![
            wire::ProviderQuota {
                profile: "work".into(),
                provider: wire::QuotaProvider::Codex,
                state: wire::QuotaState::Stale,
                fetched_at_ms: Some(1),
                windows: vec![wire::QuotaWindow {
                    id: "primary".into(),
                    label: "5h".into(),
                    remaining_basis_points: Some(7500),
                    resets_at_ms: None,
                    duration_seconds: Some(18000),
                    usage_label: None,
                }],
                issue: Some(wire::QuotaIssue {
                    code: wire::QuotaIssueCode::Timeout,
                    http_status: None,
                }),
            },
            wire::ProviderQuota {
                profile: "personal".into(),
                provider: wire::QuotaProvider::Kimi,
                state: wire::QuotaState::Unavailable,
                fetched_at_ms: None,
                windows: vec![],
                issue: Some(wire::QuotaIssue {
                    code: wire::QuotaIssueCode::Credentials,
                    http_status: None,
                }),
            },
        ];
        let mut screen = Harness::builder()
            .with_size(egui::vec2(1000.0, 220.0))
            .build_ui_state(
                |ui, profiles| {
                    ui.ctx().global_style_mut(|style| {
                        style.interaction.tooltip_delay = 0.0;
                        style.interaction.show_tooltips_only_when_still = false;
                    });
                    footer(ui, profiles);
                },
                profiles,
            );
        screen.run_steps(2);
        screen.get_by_label("Quota work").hover();
        screen.run_steps(2);
        screen.get_by_label_contains("75.0% remaining");
        screen.get_by_label_contains(issue_text(wire::QuotaIssueCode::Timeout));
        screen.get_by_label("Quota personal");
        assert!(screen.query_by_label("Refresh").is_none());
        screen.state_mut().clear();
        screen.run_steps(2);
        assert!(screen.query_by_label("Quota work").is_none());
        assert!(screen.query_by_label("Quota personal").is_none());
    }

    #[test]
    fn reset_discards_replies_from_the_previous_connection() {
        use super::super::{DiagnosticsFeature, Feature};
        let mut feature = DiagnosticsFeature::default();
        let old = feature.tag(Pending::Quota);
        feature.reset();
        let snapshot = wire::ProviderQuotas {
            profiles: vec![wire::ProviderQuota {
                profile: "old-account".into(),
                provider: wire::QuotaProvider::Codex,
                state: wire::QuotaState::Loading,
                fetched_at_ms: None,
                windows: vec![],
                issue: None,
            }],
        };
        feature.reply(
            old,
            FeatureReply::Query(Ok(QueryReply::Diagnostics(
                DiagnosticsReply::ProviderQuotas(snapshot),
            ))),
        );
        assert!(feature.quotas.profiles().is_empty());
        assert!(feature.tag(Pending::Quota) > old);
    }

    #[test]
    fn tooltip_uses_only_fixed_issue_text() {
        for code in [
            wire::QuotaIssueCode::Credentials,
            wire::QuotaIssueCode::ReauthenticationRequired,
            wire::QuotaIssueCode::Timeout,
            wire::QuotaIssueCode::Network,
            wire::QuotaIssueCode::Http,
            wire::QuotaIssueCode::Protocol,
        ] {
            let q = wire::ProviderQuota {
                profile: "work".into(),
                provider: wire::QuotaProvider::Codex,
                state: wire::QuotaState::Unavailable,
                fetched_at_ms: None,
                windows: vec![],
                issue: Some(wire::QuotaIssue {
                    code,
                    http_status: None,
                }),
            };
            assert!(tooltip(&q).contains(issue_text(code)));
        }
    }
}
