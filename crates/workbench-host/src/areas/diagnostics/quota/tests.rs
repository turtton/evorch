use super::*;
use tokio::sync::{mpsc, oneshot};
type Answer = oneshot::Sender<Result<Vec<QuotaWindow>, QuotaIssue>>;
#[derive(Debug)]
struct Call {
    profile: Profile,
    answer: Answer,
}
struct Fake {
    profile: Profile,
    calls: mpsc::UnboundedSender<Call>,
    drops: mpsc::UnboundedSender<()>,
}
struct Dropped(mpsc::UnboundedSender<()>);
impl Drop for Dropped {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
#[async_trait::async_trait]
impl Backend for Fake {
    async fn fetch(&mut self) -> Result<Vec<QuotaWindow>, QuotaIssue> {
        let _drop = Dropped(self.drops.clone());
        let (answer, result) = oneshot::channel();
        self.calls
            .send(Call {
                profile: self.profile.clone(),
                answer,
            })
            .unwrap();
        result.await.unwrap()
    }
}
#[test]
fn eligible_profiles_follow_the_existing_quota_switch() {
    let mut config = config::Config::default();
    for kind in [
        ProviderTypeConfig::OpenAiCodex,
        ProviderTypeConfig::AnthropicSubscription,
        ProviderTypeConfig::Cursor,
        ProviderTypeConfig::KimiSubscription,
        ProviderTypeConfig::OpenAi,
    ] {
        for enabled in [None, Some(false), Some(true)] {
            config.providers.insert(
                "work".into(),
                ProviderProfileConfig {
                    provider_type: kind,
                    show_quota: enabled,
                    ..Default::default()
                },
            );
            assert_eq!(
                profiles(&config).len(),
                usize::from(kind != ProviderTypeConfig::OpenAi && enabled != Some(false))
            );
        }
    }
}
fn profile(account: &str) -> Profile {
    Profile {
        name: "work".into(),
        kind: QuotaProvider::Codex,
        base_url: "https://unused.invalid".into(),
        credential: CredentialRefConfig::Keyring {
            service: "evorch".into(),
            account: account.into(),
        },
    }
}
struct World {
    service: Quotas,
    tasks: AreaTasks,
    calls: mpsc::UnboundedReceiver<Call>,
    events: mpsc::UnboundedReceiver<()>,
    drops: mpsc::UnboundedReceiver<()>,
    config: Arc<Mutex<Vec<Profile>>>,
    changed: watch::Sender<u64>,
    gate: HostRestartGate,
}
fn world() -> World {
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let (calls, received) = mpsc::unbounded_channel();
    let (events, announced) = mpsc::unbounded_channel();
    let (drops, dropped) = mpsc::unbounded_channel();
    let config = Arc::new(Mutex::new(vec![profile("first")]));
    let load = config.clone();
    let (changed, updates) = watch::channel(0);
    let gate = HostRestartGate::default();
    let service = Quotas::new(Parts {
        tasks: tasks.clone(),
        announce: Arc::new(move || {
            let _ = events.send(());
        }),
        initial: config.lock().unwrap().clone(),
        changed: updates,
        load: Arc::new(move || Some(load.lock().unwrap().clone())),
        fetch: Arc::new(move |p| {
            Ok(Box::new(Fake {
                profile: p.clone(),
                calls: calls.clone(),
                drops: drops.clone(),
            }))
        }),
        gate: gate.clone(),
    })
    .unwrap();
    World {
        service,
        tasks,
        calls: received,
        events: announced,
        drops: dropped,
        config,
        changed,
        gate,
    }
}
fn network() -> QuotaIssue {
    QuotaIssue {
        code: QuotaIssueCode::Network,
        http_status: None,
    }
}
fn windows() -> Vec<QuotaWindow> {
    vec![QuotaWindow {
        id: "primary".into(),
        label: "5h".into(),
        remaining_basis_points: Some(7500),
        resets_at_ms: None,
        duration_seconds: Some(18000),
        usage_label: None,
    }]
}
impl World {
    async fn event(&mut self) {
        self.events.recv().await.unwrap();
    }
    async fn change(&mut self, profiles: Vec<Profile>) {
        *self.config.lock().unwrap() = profiles;
        self.changed.send_modify(|n| *n += 1);
        self.event().await;
    }
}
#[tokio::test(start_paused = true)]
async fn no_query_means_no_fetch_and_the_supervisor_is_not_counted() {
    let mut w = world();
    w.event().await;
    assert!(!w.tasks.busy());
    tokio::time::advance(Duration::from_secs(600)).await;
    w.tasks.shutdown().await.unwrap();
    assert!(w.calls.try_recv().is_err());
}
#[tokio::test(start_paused = true)]
async fn two_clients_share_one_fetch_and_normal_period_is_sixty_seconds() {
    let mut w = world();
    w.event().await;
    assert_eq!(w.service.get().profiles[0].state, QuotaState::Loading);
    w.service.get();
    let call = w.calls.recv().await.unwrap();
    assert!(w.calls.try_recv().is_err());
    call.answer.send(Ok(windows())).unwrap();
    w.event().await;
    assert_eq!(w.service.snapshot().profiles[0].state, QuotaState::Ready);
    let accepted = Instant::now();
    let call = w.calls.recv().await.unwrap(); // virtual time advances to the deadline
    assert_eq!(Instant::now() - accepted, Duration::from_secs(60));
    call.answer.send(Err(network())).unwrap();
    w.event().await;
    let snapshot = w.service.snapshot();
    assert_eq!(snapshot.profiles[0].state, QuotaState::Stale);
    assert_eq!(snapshot.profiles[0].windows, windows());
    assert!(!w.tasks.busy());
    w.tasks.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn initial_failure_backoff_caps_at_six_hundred_and_success_recovers() {
    let mut w = world();
    w.event().await;
    w.service.get();
    w.calls
        .recv()
        .await
        .unwrap()
        .answer
        .send(Err(network()))
        .unwrap();
    w.event().await;
    assert_eq!(
        w.service.snapshot().profiles[0].state,
        QuotaState::Unavailable
    );
    for seconds in [120, 240, 480, 600, 600] {
        let accepted = Instant::now();
        let call = w.calls.recv().await.unwrap();
        assert_eq!(Instant::now() - accepted, Duration::from_secs(seconds));
        call.answer.send(Err(network())).unwrap();
        w.event().await;
    }
    w.calls
        .recv()
        .await
        .unwrap()
        .answer
        .send(Ok(windows()))
        .unwrap();
    w.event().await;
    assert_eq!(w.service.snapshot().profiles[0].state, QuotaState::Ready);
    let accepted = Instant::now();
    let _held = w.calls.recv().await.unwrap();
    assert_eq!(Instant::now() - accepted, Duration::from_secs(60));
    w.tasks.shutdown().await.unwrap();
    // Cancellation completed without waiting for a network answer.
}
#[tokio::test(start_paused = true)]
async fn off_cancels_inflight_on_restarts_and_account_change_cannot_publish_old_data() {
    let mut w = world();
    w.event().await;
    w.service.get();
    let old = w.calls.recv().await.unwrap();
    w.change(vec![]).await; // Off/deletion converge to the same eligible-profile list.
    assert!(w.service.snapshot().profiles.is_empty());
    w.drops.recv().await.unwrap();
    assert!(old.answer.send(Ok(windows())).is_err());
    assert!(w.calls.try_recv().is_err());
    w.change(vec![profile("first")]).await;
    let old = w.calls.recv().await.unwrap();
    w.change(vec![profile("second")]).await;
    w.drops.recv().await.unwrap();
    assert!(old.answer.send(Ok(windows())).is_err());
    let new = w.calls.recv().await.unwrap();
    assert_eq!(new.profile.credential, profile("second").credential);
    assert_eq!(w.service.snapshot().profiles[0].state, QuotaState::Loading);
    new.answer.send(Ok(vec![])).unwrap();
    w.event().await;
    assert!(w.service.snapshot().profiles[0].windows.is_empty());
    w.tasks.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn restart_gate_cancels_pending_fetch_and_reopens_without_another_query() {
    let mut w = world();
    w.event().await;
    w.service.get();
    let old = w.calls.recv().await.unwrap();
    assert!(w.gate.transition(1, HostRestartPhase::Sealing));
    w.drops.recv().await.unwrap();
    assert!(old.answer.send(Ok(windows())).is_err());
    tokio::time::advance(Duration::from_secs(600)).await;
    assert!(w.calls.try_recv().is_err());
    assert!(w.gate.transition(1, HostRestartPhase::Open));
    let resumed = w.calls.recv().await.unwrap();
    resumed.answer.send(Ok(windows())).unwrap();
    w.event().await;
    assert_eq!(w.service.snapshot().profiles[0].state, QuotaState::Ready);
    w.tasks.shutdown().await.unwrap();
}
