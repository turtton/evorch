use super::*;
use crate::diff::FixtureDiffSource;

fn request(root: &str, mode: DiffMode) -> DiffRequest {
    DiffRequest {
        repo_root: root.into(),
        mode,
    }
}

fn settle(model: &mut DiffModel, now: Instant) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while (model.worker.is_some() || model.in_flight.is_some()) && Instant::now() < deadline {
        model.poll_at(now);
        std::thread::yield_now();
    }
    assert!(model.worker.is_none());
    assert!(model.in_flight.is_none());
}

#[test]
fn visible_mode_loads_immediately_and_respects_refresh_interval() {
    let mut model = DiffModel::new();
    let mode = DiffMode::WorkingTree;
    let now = Instant::now();
    model.refresh_if_due(
        Arc::new(FixtureDiffSource::ready("first")),
        request("a", mode.clone()),
        now,
    );
    assert_eq!(model.state(&mode), &DiffState::Loading);
    settle(&mut model, now);
    let later = Arc::new(FixtureDiffSource::ready("second"));
    model.refresh_if_due(
        later.clone(),
        request("a", mode.clone()),
        now + AUTO_REFRESH_INTERVAL / 2,
    );
    assert!(model.worker.is_none(), "do not fetch every frame");
    model.refresh_if_due(
        later,
        request("a", mode.clone()),
        now + AUTO_REFRESH_INTERVAL,
    );
    assert!(
        matches!(model.state(&mode), DiffState::Ready { text } if text == "first"),
        "keep previous diff while fetching"
    );
    settle(&mut model, now + AUTO_REFRESH_INTERVAL);
    assert!(matches!(model.state(&mode), DiffState::Ready { text } if text == "second"));
}

struct GatedSource {
    entered: mpsc::Sender<DiffRequest>,
    release: std::sync::Mutex<Receiver<String>>,
}

impl DiffSource for GatedSource {
    fn fetch(&self, request: &DiffRequest) -> Result<String, DiffError> {
        self.entered.send(request.clone()).unwrap();
        Ok(self.release.lock().unwrap().recv().unwrap())
    }
}

fn gated() -> (
    Arc<GatedSource>,
    Receiver<DiffRequest>,
    mpsc::Sender<String>,
) {
    let (entered, requests) = mpsc::channel();
    let (release, responses) = mpsc::channel();
    (
        Arc::new(GatedSource {
            entered,
            release: std::sync::Mutex::new(responses),
        }),
        requests,
        release,
    )
}

#[test]
fn slow_fetches_never_overlap_even_after_manual_refresh_or_mode_switch() {
    let (source, requests, release) = gated();
    let mut model = DiffModel::new();
    let now = Instant::now();
    model.request(source.clone(), request("a", DiffMode::WorkingTree));
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..10 {
        model.request(source.clone(), request("a", DiffMode::WorkingTree));
        model.refresh_if_due(
            source.clone(),
            request("a", DiffMode::Branch),
            now + Duration::from_secs(20),
        );
    }
    assert!(requests.try_recv().is_err());
    release.send("working".into()).unwrap();
    settle(&mut model, now);
    // The newly selected mode gets its first fetch as soon as the worker is free.
    model.refresh_if_due(source, request("a", DiffMode::Branch), now);
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(5)).unwrap().mode,
        DiffMode::Branch
    );
    release.send("branch".into()).unwrap();
    settle(&mut model, now);
    assert!(
        matches!(model.state(&DiffMode::Branch), DiffState::Ready { text } if text == "branch")
    );
}

#[test]
fn repository_changes_clear_both_scopes_and_discard_old_worker_results() {
    let now = Instant::now();
    let mut model = DiffModel::new();
    model.request(
        Arc::new(FixtureDiffSource::ready("old branch")),
        request("a", DiffMode::Branch),
    );
    settle(&mut model, now);
    let (source, requests, release) = gated();
    model.request(source.clone(), request("a", DiffMode::WorkingTree));
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    model.set_repo_root(Some("b".into()));
    assert_eq!(model.state(&DiffMode::Branch), &DiffState::Idle);
    assert_eq!(model.state(&DiffMode::WorkingTree), &DiffState::Idle);
    release.send("old repo".into()).unwrap();
    settle(&mut model, now);
    assert_eq!(model.state(&DiffMode::WorkingTree), &DiffState::Idle);
    model.refresh_if_due(source, request("b", DiffMode::WorkingTree), now);
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .repo_root,
        PathBuf::from("b")
    );
    release.send("new repo".into()).unwrap();
    settle(&mut model, now);
    assert!(
        matches!(model.state(&DiffMode::WorkingTree), DiffState::Ready { text } if text == "new repo")
    );
    model.set_repo_root(None);
    assert_eq!(model.state(&DiffMode::WorkingTree), &DiffState::Idle);
}

#[test]
fn refresh_failure_keeps_previous_diff_and_backs_off_then_recovers() {
    let now = Instant::now();
    let mode = DiffMode::WorkingTree;
    let mut model = DiffModel::new();
    model.request(
        Arc::new(FixtureDiffSource::ready("previous")),
        request("a", mode.clone()),
    );
    settle(&mut model, now);
    model.request(
        Arc::new(FixtureDiffSource::error("unavailable")),
        request("a", mode.clone()),
    );
    settle(&mut model, now);
    assert!(model.refresh_error(&mode).unwrap().contains("unavailable"));
    assert!(matches!(model.state(&mode), DiffState::Ready { text } if text == "previous"));
    let recovered = Arc::new(FixtureDiffSource::ready("recovered"));
    model.refresh_if_due(
        recovered.clone(),
        request("a", mode.clone()),
        now + AUTO_REFRESH_INTERVAL,
    );
    assert!(model.worker.is_none());
    model.refresh_if_due(
        recovered,
        request("a", mode.clone()),
        now + ERROR_RETRY_INTERVAL,
    );
    settle(&mut model, now + ERROR_RETRY_INTERVAL);
    assert!(model.refresh_error(&mode).is_none());
    assert!(matches!(model.state(&mode), DiffState::Ready { text } if text == "recovered"));
}

#[test]
fn restored_snapshot_is_not_replaced_by_live_or_in_flight_diff() {
    let now = Instant::now();
    let mode = DiffMode::WorkingTree;
    let mut model = DiffModel::new();
    let (source, requests, release) = gated();
    model.request(source, request("a", mode.clone()));
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    model.show_snapshot("restored".into());
    release.send("stale".into()).unwrap();
    settle(&mut model, now);
    let live = Arc::new(FixtureDiffSource::ready("live"));
    model.refresh_if_due(
        live.clone(),
        request("a", mode.clone()),
        now + Duration::from_secs(100),
    );
    assert!(model.worker.is_none());
    assert!(matches!(model.state(&mode), DiffState::Ready { text } if text == "restored"));
    model.request(live, request("a", mode.clone()));
    settle(&mut model, now);
    assert!(!model.is_snapshot());
    assert!(matches!(model.state(&mode), DiffState::Ready { text } if text == "live"));
}
