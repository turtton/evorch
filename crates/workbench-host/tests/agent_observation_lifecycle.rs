//! Lifecycle of the host's agent observation source (#707) against a REAL
//! Chromium, the real agent browser, and a real runtime that owns the run.
//!
//! Nothing here is faked: the source under test is `AgentObservationSource`
//! over the browser the agent's own page lives in. What is NOT covered is the
//! production composition (bootstrap, the model's browser tool, the service's
//! authority); the transport tests of evorch-core use a fake source and the
//! service tests drive the authority.
//!
//! Chromium tests are `#[ignore]` and run with `--run-ignored only` in the CI
//! job that installs Chromium (`CHROME` names its executable). The tests that
//! need no browser (`current()` is not an authorization) run everywhere.
//!
//! Waits are events: a page that appears on a watch channel, a channel that
//! ends, a task registry that joins. No wait is a duration.
#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;

use browser::agent::{
    AgentBrowser, LaunchOptions, PageProvider,
    observation::{
        ExecutionStamp,
        source::{ExecutionPage, Feed as UpstreamFeed, Publication},
        with_execution_stamp,
    },
};
use serde_json::json;
use workbench_host::areas::{
    AreaTasks,
    browser::observation::{
        AgentObservationPort, ObservationFailure, ObservationFeed, ObservedPage,
        source::{
            AgentObservationSource,
            gate::{GatePoint, SourceGate},
        },
    },
};
use workbench_protocol::{
    ids::{ProjectId, ThreadId},
    namespaces::browser::observation::{MAX_RUNS, ObservationScope},
};

const THREAD: &str = "thread-1";

fn project() -> ProjectId {
    common::project()
}

fn thread() -> ThreadId {
    ThreadId::new(THREAD).unwrap()
}

/// The production network policy; a `data:` page never reaches the network.
fn launch() -> LaunchOptions {
    LaunchOptions {
        executable: None,
        policy: Arc::new(tools::GuardPolicy(Arc::new(
            tools::NetworkGuard::new().unwrap(),
        ))),
        max_pages: 4,
        max_tabs: 4,
    }
}

/// A run in flight (its model never answers), its thread stored under the
/// project, one agent browser and the source over them.
struct World {
    _dir: tempfile::TempDir,
    _storage: storage::Storage,
    runtime: runtime::AgentRuntime,
    browser: Arc<AgentBrowser>,
    tasks: AreaTasks,
    source: AgentObservationSource,
    run: runtime::RunId,
    incarnation: String,
}

impl World {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, storage) = common::runtime_with_store(dir.path());
        let run = runtime
            .delegate_chat(
                THREAD,
                runtime::Role::Worker,
                "observe".into(),
                runtime::RunConfig::default(),
            )
            .unwrap();
        storage
            .handle()
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: THREAD.into(),
                project_id: Some(project().to_string()),
                project_root: Some(dir.path().to_string_lossy().into_owned()),
                title: Some("observed".into()),
                root_run_id: Some(run.to_string()),
                ..Default::default()
            }])
            .unwrap();
        let browser = Arc::new(AgentBrowser::new(launch()));
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let source = AgentObservationSource::new(
            browser.clone(),
            runtime.browser_execution_reader(),
            7,
            tasks.clone(),
        );
        let incarnation = runtime
            .browser_execution_reader()
            .observe(run)
            .expect("the run is in flight")
            .incarnation;
        Self {
            _dir: dir,
            _storage: storage,
            runtime,
            browser,
            tasks,
            source,
            run,
            incarnation,
        }
    }

    fn stamp(&self) -> Arc<ExecutionStamp> {
        Arc::new(ExecutionStamp {
            run: self.run.to_string(),
            incarnation: self.incarnation.clone(),
        })
    }

    /// The agent's first page, created as the browser tools create it: under
    /// the run's creation stamp.
    async fn open(&self, url: &str) -> Arc<browser::agent::PageAgent> {
        let page = with_execution_stamp(self.stamp(), self.browser.page(&self.run.to_string()))
            .await
            .unwrap();
        page.navigate(url).await.unwrap();
        page
    }

    fn execution(&self) -> ExecutionPage {
        self.browser
            .observation_pages()
            .into_iter()
            .find(|page| page.stamp.run == self.run.to_string())
            .expect("the run has a page")
    }

    fn scope(&self) -> ObservationScope {
        self.source
            .list(&project(), &thread())
            .unwrap()
            .pop()
            .expect("the run's page is listed")
    }
}

/// The next publication of the upstream feed that satisfies `predicate`.
async fn until(
    feed: &mut UpstreamFeed,
    predicate: impl Fn(&Publication) -> bool,
) -> Arc<Publication> {
    loop {
        if let Some(page) = feed
            .updates
            .borrow_and_update()
            .as_ref()
            .filter(|page| predicate(page))
            .cloned()
        {
            return page;
        }
        feed.updates.changed().await.expect("the upstream is live");
    }
}

/// The next page the host's feed carries that satisfies `predicate`.
async fn host_until(
    feed: &mut ObservationFeed,
    predicate: impl Fn(&ObservedPage) -> bool,
) -> Arc<ObservedPage> {
    loop {
        if let Some(page) = feed
            .updates
            .borrow_and_update()
            .as_ref()
            .filter(|page| predicate(page))
            .cloned()
        {
            return page;
        }
        feed.updates.changed().await.expect("the host feed is live");
    }
}

const RED: &str =
    "data:text/html,<title>Red</title><body style=\"background:rgb(255,0,0)\">red</body>";
const BLUE: &str =
    "data:text/html,<title>Blue</title><body style=\"background:rgb(0,0,255)\">blue</body>";

/// The pixel in the middle of a frame.
fn centre(frame: &workbench_host::areas::browser::observation::ObservedFrame) -> [u8; 3] {
    let (width, height) = (frame.start.width, frame.start.height);
    let at = ((height / 2 * width + width / 2) * 4) as usize;
    [frame.rgba[at], frame.rgba[at + 1], frame.rgba[at + 2]]
}

fn is_red(pixel: [u8; 3]) -> bool {
    pixel[0] > 120 && pixel[1] < 80 && pixel[2] < 80
}

fn is_blue(pixel: [u8; 3]) -> bool {
    pixel[2] > 120 && pixel[0] < 80 && pixel[1] < 80
}

/// The colours the tests paint a page with, one after another. None of them
/// is the red the pages start with.
fn palette(n: usize) -> [u8; 3] {
    [
        [0, 0, 255],
        [0, 255, 0],
        [255, 255, 0],
        [255, 0, 255],
        [0, 255, 255],
    ][n % 5]
}

/// Whether `pixel` is `rgb` as a JPEG screencast shows it.
fn near(pixel: [u8; 3], rgb: [u8; 3]) -> bool {
    pixel.iter().zip(rgb).all(|(a, b)| a.abs_diff(b) < 90)
}

/// Paints the page with `rgb` without navigating it (a navigation under
/// observation is not what these tests are about).
async fn paint(page: &browser::agent::PageAgent, rgb: [u8; 3]) {
    let [r, g, b] = rgb;
    page.target()
        .call(
            "Runtime.evaluate",
            json!({ "expression": format!(
                "document.documentElement.style.background = 'rgb({r},{g},{b})'; document.body.style.background = 'rgb({r},{g},{b})'"
            ) }),
        )
        .await
        .unwrap();
}

/// Paints the page and waits until the host's feed carries its pixels. New
/// pixels can only reach the feed through the source's own task, so this also
/// proves the task has joined the upstream.
async fn repaint(
    page: &browser::agent::PageAgent,
    feed: &mut ObservationFeed,
    rgb: [u8; 3],
) -> Arc<ObservedPage> {
    paint(page, rgb).await;
    host_until(feed, move |page| {
        page.frame.as_ref().is_some_and(|f| near(centre(f), rgb))
    })
    .await
}

/// A gate of the source's task that reports every arrival to the test and
/// holds the task until the test releases it. The task of a source without
/// this gate never waits.
struct Held {
    arrivals: tokio::sync::mpsc::UnboundedSender<(GatePoint, tokio::sync::oneshot::Sender<()>)>,
}
type Arrivals = tokio::sync::mpsc::UnboundedReceiver<(GatePoint, tokio::sync::oneshot::Sender<()>)>;
impl SourceGate for Held {
    fn wait(
        &self,
        point: GatePoint,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let (release, released) = tokio::sync::oneshot::channel();
        if self.arrivals.send((point, release)).is_err() {
            return Box::pin(async {});
        }
        Box::pin(async move {
            let _ = released.await;
        })
    }
}

/// A source over `world`'s browser and runtime whose task waits at the gate.
fn gated(world: &World, tasks: &AreaTasks) -> (AgentObservationSource, Arrivals) {
    let (arrivals, receiver) = tokio::sync::mpsc::unbounded_channel();
    let source = AgentObservationSource::new(
        world.browser.clone(),
        world.runtime.browser_execution_reader(),
        7,
        tasks.clone(),
    )
    .with_gate_for_test(Arc::new(Held { arrivals }));
    (source, receiver)
}

/// The next arrival at the gate, which must be at `expected`; the task waits
/// until the returned sender is used.
async fn arrives(arrivals: &mut Arrivals, expected: GatePoint) -> tokio::sync::oneshot::Sender<()> {
    let (point, release) = arrivals.recv().await.expect("the source outlives the test");
    assert_eq!(point, expected);
    release
}

/// Waits until the task is past the point it is at.
async fn passes(arrivals: &mut Arrivals, expected: GatePoint) {
    arrives(arrivals, expected).await.send(()).unwrap();
}

// ---- #707 / plan section 7 -------------------------------------------------

/// The first subscriber starts the producer, once. Later subscribers of the
/// same run share that one task, its one upstream feed and its pages.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn first_subscriber_starts_the_producer_once_and_later_subscribers_share_it() {
    let world = World::new().await;
    let _page = world.open(RED).await;
    let scope = world.scope();
    assert_eq!(world.tasks.active(), 0, "listing starts nothing");

    let mut first = world.source.subscribe(&scope).unwrap();
    assert_eq!(world.tasks.active(), 1);
    let mut second = world.source.subscribe(&scope).unwrap();
    let mut third = world.source.subscribe(&scope).unwrap();
    assert_eq!(
        world.tasks.active(),
        1,
        "one task, however many subscribers"
    );
    assert!(first.updates.same_channel(&second.updates));
    assert!(first.updates.same_channel(&third.updates));

    for feed in [&mut first, &mut second, &mut third] {
        host_until(feed, |page| page.frame.is_some()).await;
    }
    let seen = [&first, &second, &third].map(|feed| feed.updates.borrow().clone().unwrap());
    assert!(
        Arc::ptr_eq(&seen[0], &seen[1]) && Arc::ptr_eq(&seen[0], &seen[2]),
        "every subscriber reads the one conversion of the one publication"
    );
    drop((first, second, third));
    world.tasks.shutdown().await.unwrap();
}

/// The last subscriber's leaving retires the source's channel, with no stop
/// signal from the host; the ones before it do not. The task then closes the
/// upstream and the host's registry joins it. A later registry gets a new
/// producer. The test is synchronised on the task's own gate: it knows when
/// the channel is ended and when it left the registry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn last_subscriber_leaving_stops_the_producer_and_a_later_one_starts_anew() {
    let world = World::new().await;
    let page = world.open(RED).await;
    let scope = world.scope();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let (source, mut arrivals) = gated(&world, &tasks);
    let mut first = source.subscribe(&scope).unwrap();
    let mut second = source.subscribe(&scope).unwrap();
    passes(&mut arrivals, GatePoint::BeforeUpstream).await;
    host_until(&mut first, |page| page.frame.is_some()).await;

    // one subscriber is left: the channel keeps serving it, pixels included,
    // and nothing is retired
    drop(first);
    repaint(&page, &mut second, palette(1)).await;
    assert_eq!(tasks.active(), 1, "the one task still serves it");
    assert!(
        source.snapshot(&scope).unwrap().frame.is_some(),
        "a snapshot is served from the live channel"
    );
    assert!(arrivals.try_recv().is_err(), "nothing was retired yet");

    // the last one leaves: the task ends its channel on its own
    drop(second);
    let retiring = arrives(&mut arrivals, GatePoint::Retiring).await;
    retiring.send(()).unwrap();
    let retired = arrives(&mut arrivals, GatePoint::Retired).await;
    assert!(
        source.snapshot(&scope).unwrap().frame.is_none(),
        "the channel left the registry: a snapshot is built from the page again"
    );
    retired.send(()).unwrap();
    tasks.shutdown().await.unwrap();
    assert_eq!(tasks.active(), 0, "joined");

    // whoever observes next starts a producer of its own
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let source = AgentObservationSource::new(
        world.browser.clone(),
        world.runtime.browser_execution_reader(),
        7,
        tasks.clone(),
    );
    let mut again = source.subscribe(&scope).unwrap();
    repaint(&page, &mut again, palette(2)).await;
    drop(again);
    tasks.shutdown().await.unwrap();
    assert_eq!(tasks.active(), 0);
}

/// A subscription that cannot start, or is cut at its start, leaves neither a
/// task nor a producer (a lease on the page) behind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn creation_cancelled_failed_or_refused_leaves_no_task_and_no_lease() {
    let world = World::new().await;
    let _page = world.open(RED).await;
    let scope = world.scope();
    let source_over = |tasks: &AreaTasks| {
        AgentObservationSource::new(
            world.browser.clone(),
            world.runtime.browser_execution_reader(),
            7,
            tasks.clone(),
        )
    };

    // refused before anything starts: the host is stopping
    let stopping = AreaTasks::new(tokio::runtime::Handle::current());
    let refused = source_over(&stopping);
    stopping.request_stop();
    assert_eq!(
        refused.subscribe(&scope).err(),
        Some(ObservationFailure::Capacity)
    );
    assert_eq!(stopping.active(), 0);
    stopping.shutdown().await.unwrap();

    // cut at its start: the stop arrives right after the subscription was
    // accepted. The task ends with its channel and releases the upstream.
    let cut = AreaTasks::new(tokio::runtime::Handle::current());
    let source = source_over(&cut);
    let mut feed = source.subscribe(&scope).unwrap();
    cut.request_stop();
    cut.shutdown().await.unwrap();
    assert_eq!(cut.active(), 0);
    feed.updates.wait_for(|page| page.is_none()).await.unwrap();
    drop(feed);

    // the page is free for the next owner: a new source gets a live feed
    // (a producer left behind would be shared, and still bound to the old one)
    let next = AreaTasks::new(tokio::runtime::Handle::current());
    let source = source_over(&next);
    let mut feed = source.subscribe(&scope).unwrap();
    host_until(&mut feed, |page| page.frame.is_some()).await;

    next.shutdown().await.unwrap();
    assert_eq!(next.active(), 0);
    drop(feed);

    // the upstream cannot be taken: the subscription was accepted and its task
    // admitted, and the page ends before the task asks the browser for the
    // upstream (the gate holds the task exactly there)
    let failing = AreaTasks::new(tokio::runtime::Handle::current());
    let (source, mut arrivals) = gated(&world, &failing);
    let mut feed = source.subscribe(&scope).unwrap();
    assert_eq!(failing.active(), 1);
    let release = arrives(&mut arrivals, GatePoint::BeforeUpstream).await;
    assert!(world.browser.close(&world.run.to_string()).await);
    release.send(()).unwrap();
    // the failure path ends the channel and leaves the registry
    feed.updates.wait_for(|page| page.is_none()).await.unwrap();
    assert_eq!(
        source.snapshot(&scope).err(),
        Some(ObservationFailure::Revoked),
        "nothing is served for a page that is gone, and no channel is left"
    );
    assert_eq!(
        source.subscribe(&scope).err(),
        Some(ObservationFailure::Revoked)
    );
    // no producer was ever started and the task is gone
    failing.shutdown().await.unwrap();
    assert_eq!(failing.active(), 0);
    assert!(
        arrivals.try_recv().is_err(),
        "the failed task does not reach the retirement points"
    );
}

/// A creation that fails leaves nothing in the registry: the source holds at
/// most `MAX_RUNS` channels, so an entry left behind by each failed creation
/// would refuse the subscription after that many.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn failed_creations_leave_no_entry_in_the_registry() {
    let world = World::new().await;
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let (source, mut arrivals) = gated(&world, &tasks);
    for round in 0..=MAX_RUNS {
        let _page = world.open(RED).await;
        let scope = world.scope();
        let mut feed = source
            .subscribe(&scope)
            .unwrap_or_else(|failure| panic!("round {round}: {failure:?}"));
        let release = arrives(&mut arrivals, GatePoint::BeforeUpstream).await;
        assert!(world.browser.close(&world.run.to_string()).await);
        release.send(()).unwrap();
        feed.updates.wait_for(|page| page.is_none()).await.unwrap();
    }
    tasks.shutdown().await.unwrap();
    assert_eq!(tasks.active(), 0);
}

/// ABA, in a fixed order. The old channel's task has ended its channel and is
/// held before it leaves the registry; a new subscriber registers a new
/// channel meanwhile; the old task is released. Its cleanup must not remove
/// the entry of the channel that replaced it: a third subscriber attaches to
/// that channel, no new task starts, and the channel keeps serving.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn old_cleanup_finishing_after_a_new_channel_registered_does_not_remove_it() {
    let world = World::new().await;
    let page = world.open(RED).await;
    let scope = world.scope();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let (source, mut arrivals) = gated(&world, &tasks);

    // channel 1 is live, then its only subscriber leaves
    let mut first = source.subscribe(&scope).unwrap();
    passes(&mut arrivals, GatePoint::BeforeUpstream).await;
    host_until(&mut first, |page| page.frame.is_some()).await;
    drop(first);
    // its task ended the channel and is held before it leaves the registry
    let old_cleanup = arrives(&mut arrivals, GatePoint::Retiring).await;

    // channel 2 registers meanwhile, with a task of its own
    let mut second = source.subscribe(&scope).unwrap();
    passes(&mut arrivals, GatePoint::BeforeUpstream).await;
    host_until(&mut second, |page| page.frame.is_some()).await;
    assert_eq!(tasks.active(), 2, "the old task is still closing");

    // the old task finishes its cleanup
    old_cleanup.send(()).unwrap();
    passes(&mut arrivals, GatePoint::Retired).await;

    // channel 2 is still the registered one
    let mut third = source.subscribe(&scope).unwrap();
    assert!(
        second.updates.same_channel(&third.updates),
        "the old cleanup removed the entry of the new channel"
    );
    assert!(
        arrivals.try_recv().is_err(),
        "no task was started for the third subscriber"
    );
    // and it keeps serving both
    repaint(&page, &mut second, palette(3)).await;
    host_until(&mut third, |page| {
        page.frame
            .as_ref()
            .is_some_and(|f| near(centre(f), palette(3)))
    })
    .await;
    drop((second, third));
    passes(&mut arrivals, GatePoint::Retiring).await;
    passes(&mut arrivals, GatePoint::Retired).await;
    tasks.shutdown().await.unwrap();
    assert_eq!(tasks.active(), 0);
}

/// The next page of the host's feed that satisfies `predicate`; `None` when the
/// feed ended first.
async fn host_until_or_end(
    feed: &mut ObservationFeed,
    predicate: impl Fn(&ObservedPage) -> bool,
) -> Option<Arc<ObservedPage>> {
    loop {
        if let Some(page) = feed
            .updates
            .borrow_and_update()
            .as_ref()
            .filter(|page| predicate(page))
            .cloned()
        {
            return Some(page);
        }
        if feed.updates.changed().await.is_err() {
            return None;
        }
    }
}

/// Subscribing again right after the previous subscriber left must end in a
/// live feed: if it is attached to a channel that is being torn down it ends
/// visibly (it is closed, never silently stale) and the next subscribe is
/// served by a new channel. The entry that serves a live feed stays the
/// registered one: a finished teardown does not remove it.
///
/// Reading the code, a subscriber can in theory be handed the channel of a
/// task that has decided to stop (`tx.closed()`) but has not yet published
/// `None`. That window was not observed in 20 runs under CPU pressure, so this
/// test pins the contract that holds whether or not the window is hit: a feed
/// that ends is visibly ended, and the next subscribe is live and registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn resubscribing_during_teardown_ends_in_a_live_registered_feed() {
    let world = World::new().await;
    let page = world.open(RED).await;
    let scope = world.scope();
    for round in 0..24 {
        drop(world.source.subscribe(&scope).unwrap());
        let mut attempts = 0;
        let mut feed = loop {
            let mut feed = world.source.subscribe(&scope).unwrap();
            let rgb = palette(round + attempts);
            paint(&page, rgb).await;
            if host_until_or_end(&mut feed, |page| {
                page.frame.as_ref().is_some_and(|f| near(centre(f), rgb))
            })
            .await
            .is_some()
            {
                break feed;
            }
            attempts += 1;
            assert!(
                attempts < 50,
                "round {round}: no live feed in {attempts} attempts"
            );
        };
        // the live channel is the registered one
        let mut other = world.source.subscribe(&scope).unwrap();
        assert!(feed.updates.same_channel(&other.updates), "round {round}");
        assert!(feed.updates.has_changed().is_ok(), "round {round}");
        let _ = (&mut feed, &mut other);
    }
    world.tasks.shutdown().await.unwrap();
}

fn title_of(page: &ObservedPage) -> &str {
    page.snapshot
        .tabs
        .iter()
        .find(|tab| Some(&tab.id) == page.snapshot.active_tab.as_ref())
        .map_or("", |tab| tab.title.as_str())
}

/// Navigating the page of a run right after a subscription started its
/// producer keeps the observation alive. `Page.startScreencast` can be refused
/// ("Not attached to an active page") while the page navigates; that must be
/// retried, not taken as the end of the observation. Every navigation is
/// followed by a frame that can only be the new page's: the page is painted in
/// a colour of its own, and the feed must carry that colour with the new title.
///
/// The race shows only under CPU pressure, so several browsers navigate at
/// once; run it also under
/// `scripts/reproduce-under-limits.sh --cpus 1 --stress-cpus 2`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn navigating_an_observed_page_keeps_the_observation_alive() {
    async fn subscribe_and_navigate(round_count: usize) {
        let world = World::new().await;
        let page = world.open(RED).await;
        let scope = world.scope();
        for round in 0..round_count {
            // the subscription starts the producer and the page is navigated
            // before the producer has published its first frame
            let mut feed = world.source.subscribe(&scope).unwrap();
            let title = format!("navigated-{round}");
            let rgb = palette(round);
            let [r, g, b] = rgb;
            page.navigate(&format!(
                "data:text/html,<title>{title}</title><body style=\"background:rgb({r},{g},{b})\">page</body>"
            ))
            .await
            .unwrap();
            host_until(&mut feed, |page| {
                title_of(page) == title && page.frame.as_ref().is_some_and(|f| near(centre(f), rgb))
            })
            .await;
        }
        world.tasks.shutdown().await.unwrap();
    }
    tokio::join!(
        subscribe_and_navigate(12),
        subscribe_and_navigate(12),
        subscribe_and_navigate(12),
    );
}

/// A run's end, a re-subscription after it, and a new incarnation of the same
/// run never deliver the old frame again. The old page keeps its old stamp
/// and is refused; the page created for the new incarnation is a new scope
/// that starts from its own first frame.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn terminal_state_resubscription_and_new_incarnation_never_redeliver_an_old_frame() {
    let world = World::new().await;
    let _page = world.open(RED).await;
    let old = world.scope();
    let mut feed = world.source.subscribe(&old).unwrap();
    let first = host_until(&mut feed, |page| page.frame.is_some()).await;
    assert!(is_red(centre(first.frame.as_ref().unwrap())));

    // terminal: the run is stopped
    world.runtime.cancel(world.run).unwrap();
    world.runtime.wait(world.run).await.unwrap();
    feed.updates.wait_for(|page| page.is_none()).await.unwrap();
    assert!(!world.source.current(&old));
    assert_eq!(
        world.source.subscribe(&old).err(),
        Some(ObservationFailure::Revoked)
    );
    assert_eq!(
        world.source.snapshot(&old).err(),
        Some(ObservationFailure::Revoked)
    );
    assert!(world.source.list(&project(), &thread()).unwrap().is_empty());

    // a new incarnation of the same run
    assert_eq!(
        world
            .runtime
            .continue_goal(world.run, "again".into(), runtime::RunConfig::default())
            .unwrap(),
        world.run
    );
    let incarnation = world
        .runtime
        .browser_execution_reader()
        .observe(world.run)
        .expect("the run is in flight again")
        .incarnation;
    assert_ne!(incarnation, world.incarnation);
    // its old page still carries the old stamp and is refused
    assert!(!world.source.current(&old));
    assert_eq!(
        world.source.subscribe(&old).err(),
        Some(ObservationFailure::Revoked)
    );
    assert!(world.source.list(&project(), &thread()).unwrap().is_empty());

    // the page of the new incarnation: another stamp, another generation
    assert!(world.browser.close(&world.run.to_string()).await);
    let stamp = Arc::new(ExecutionStamp {
        run: world.run.to_string(),
        incarnation,
    });
    let page = with_execution_stamp(stamp, world.browser.page(&world.run.to_string()))
        .await
        .unwrap();
    page.navigate(BLUE).await.unwrap();
    let scopes = world.source.list(&project(), &thread()).unwrap();
    assert_eq!(scopes.len(), 1);
    let new = scopes[0].clone();
    assert_ne!(new, old);
    assert_ne!(new.incarnation, old.incarnation);
    assert_ne!(new.page_generation, old.page_generation);
    assert_eq!(
        world.source.subscribe(&old).err(),
        Some(ObservationFailure::Revoked),
        "the old scope is not revived by the new page"
    );
    let mut fresh = world.source.subscribe(&new).unwrap();
    let blue = host_until(&mut fresh, |page| {
        page.frame.as_ref().is_some_and(|f| is_blue(centre(f)))
    })
    .await;
    assert_eq!(blue.snapshot.scope, new);
    assert_eq!(blue.frame.as_ref().unwrap().start.scope, new);
    world.runtime.cancel(world.run).unwrap();
    world.runtime.wait(world.run).await.unwrap();
    world.tasks.shutdown().await.unwrap();
}

/// Observing is not owning: when every subscriber has left, the agent's
/// page is still registered, current and usable, and the producer's stop did
/// not close its tab.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn observation_ending_does_not_take_the_agent_page() {
    let world = World::new().await;
    let page = world.open(RED).await;
    let scope = world.scope();
    let execution = world.execution();
    let mut feed = world.source.subscribe(&scope).unwrap();
    host_until(&mut feed, |page| page.frame.is_some()).await;
    drop(feed);
    world.tasks.shutdown().await.unwrap();
    assert_eq!(world.tasks.active(), 0, "the observation is over");

    assert!(world.browser.observation_current(&execution));
    assert!(world.source.current(&scope));
    let same = world
        .browser
        .open_page(&world.run.to_string())
        .await
        .expect("the agent still has its page");
    assert!(Arc::ptr_eq(&same, &page));
    same.navigate(BLUE).await.unwrap();

    // and it can be observed again by whoever owns the next registry
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let source = AgentObservationSource::new(
        world.browser.clone(),
        world.runtime.browser_execution_reader(),
        7,
        tasks.clone(),
    );
    let mut again = source.subscribe(&scope).unwrap();
    host_until(&mut again, |page| {
        page.frame.as_ref().is_some_and(|f| is_blue(centre(f)))
    })
    .await;
    drop(again);
    tasks.shutdown().await.unwrap();
    assert!(
        world.browser.observation_current(&execution),
        "not even the end of the second observation closes the page"
    );
}

// ---- review additions ------------------------------------------------------

/// A page that never repaints has one frame, ever. A subscriber still gets it:
/// from a producer that this subscription starts, and from a producer that
/// already published it before this subscriber came.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn static_page_delivers_its_only_frame_to_a_fresh_and_to_a_late_subscriber() {
    let world = World::new().await;
    let _page = world.open(RED).await;
    let scope = world.scope();

    // a fresh producer
    let mut fresh = world.source.subscribe(&scope).unwrap();
    let page = host_until(&mut fresh, |page| page.frame.is_some()).await;
    assert!(is_red(centre(page.frame.as_ref().unwrap())));
    drop(fresh);
    // joined, so that nothing of the first producer is left to join
    world.tasks.shutdown().await.unwrap();

    // a producer that already published its only frame: held by a consumer
    // of the test while the source subscribes and the page stays still
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let source = AgentObservationSource::new(
        world.browser.clone(),
        world.runtime.browser_execution_reader(),
        7,
        tasks.clone(),
    );
    let mut held = world.browser.observe(&world.execution()).unwrap();
    until(&mut held, |p| p.frame.is_some()).await;
    let mut late = source.subscribe(&scope).unwrap();
    let page = host_until(&mut late, |page| page.frame.is_some()).await;
    assert!(is_red(centre(page.frame.as_ref().unwrap())));
    drop(late);
    tasks.shutdown().await.unwrap();
    held.close().await;
}

/// Shutdown joins the source's task and releases the upstream; a host that
/// was shut down starts nothing new.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn host_shutdown_joins_the_source_task_and_releases_the_upstream() {
    let world = World::new().await;
    let _page = world.open(RED).await;
    let scope = world.scope();
    let mut feed = world.source.subscribe(&scope).unwrap();
    host_until(&mut feed, |page| page.frame.is_some()).await;
    assert_eq!(world.tasks.active(), 1);

    world.tasks.shutdown().await.unwrap();

    assert_eq!(world.tasks.active(), 0, "joined");
    feed.updates.wait_for(|page| page.is_none()).await.unwrap();
    assert_eq!(
        world.source.subscribe(&scope).err(),
        Some(ObservationFailure::Capacity),
        "a stopped host starts nothing"
    );
    assert!(world.browser.observation_current(&world.execution()));
    // the producer was the host's last consumer and is gone: a consumer of
    // the test starts a new one
    let mut next = world.browser.observe(&world.execution()).unwrap();
    until(&mut next, |p| p.frame.is_some()).await;
    next.close().await;
}

/// The run's execution is invalidated while observed: the source's task ends
/// by itself, its feed says so, without the host shutting anything down; the
/// registry then joins it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn runtime_invalidation_ends_the_source_task_by_itself_and_shutdown_joins_it() {
    let world = World::new().await;
    let _page = world.open(RED).await;
    let scope = world.scope();
    let mut feed = world.source.subscribe(&scope).unwrap();
    host_until(&mut feed, |page| page.frame.is_some()).await;

    world.runtime.cancel(world.run).unwrap();
    world.runtime.wait(world.run).await.unwrap();

    feed.updates.wait_for(|page| page.is_none()).await.unwrap();
    assert!(!world.source.current(&scope));
    world.tasks.shutdown().await.unwrap();
    assert_eq!(world.tasks.active(), 0);
    assert_eq!(
        world.source.subscribe(&scope).err(),
        Some(ObservationFailure::Revoked)
    );
}

/// Switching the active tab never publishes the new tab's metadata with the
/// old tab's pixels, nor the reverse: in every publication the frame belongs
/// to the active tab and shows that tab's page.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn active_tab_switch_never_mixes_new_metadata_with_old_pixels() {
    let world = World::new().await;
    let page = world.open(RED).await;
    let scope = world.scope();
    page.target()
        .call(
            "Runtime.evaluate",
            json!({ "expression": "window.open('about:blank')" }),
        )
        .await
        .unwrap();
    let mut feed = world.source.subscribe(&scope).unwrap();
    let both = host_until(&mut feed, |page| page.snapshot.tabs.len() == 2).await;
    let tab_where = |red: bool| {
        both.snapshot
            .tabs
            .iter()
            .find(|tab| (tab.title == "Red") == red)
            .map(|tab| tab.id.clone())
            .expect("both tabs are listed")
    };
    let (red_tab, blue_tab) = (tab_where(true), tab_where(false));
    // the browser names tabs its own way; its listing carries the same urls
    let tabs = world.browser.tabs(&world.run.to_string()).await.unwrap();
    let browser_tab = |red: bool| {
        tabs.iter()
            .find(|tab| tab.url.contains("<title>Red</title>") == red)
            .map(|tab| tab.id.clone())
            .expect("the browser lists both tabs")
    };
    let (red_name, blue_name) = (browser_tab(true), browser_tab(false));
    // the popup is blank until it is painted blue, as the agent would paint it
    let popup = world
        .browser
        .switch_tab(&world.run.to_string(), &blue_name)
        .await
        .unwrap();
    popup
        .target()
        .call(
            "Runtime.evaluate",
            json!({ "expression": "document.documentElement.style.background = 'rgb(0,0,255)'" }),
        )
        .await
        .unwrap();

    // every publication that is seen along the way must be coherent
    let coherent = |page: &ObservedPage| {
        assert!(page.validate(), "a publication is whole");
        if let Some(frame) = &page.frame {
            assert_eq!(Some(&frame.start.tab), page.snapshot.active_tab.as_ref());
            let pixel = centre(frame);
            // a tab may still be unpainted (white) or compressed oddly; what it must
            // never show is the other tab's colour
            if frame.start.tab == red_tab {
                assert!(
                    !is_blue(pixel),
                    "red tab with the blue tab's pixels {pixel:?}"
                );
            } else if frame.start.tab == blue_tab {
                assert!(
                    !is_red(pixel),
                    "blue tab with the red tab's pixels {pixel:?}"
                );
            }
        }
    };
    for (name, tab, wanted_blue) in [
        (&blue_name, &blue_tab, true),
        (&red_name, &red_tab, false),
        (&blue_name, &blue_tab, true),
        (&red_name, &red_tab, false),
    ] {
        world
            .browser
            .switch_tab(&world.run.to_string(), name)
            .await
            .unwrap();
        let settled = host_until(&mut feed, |page| {
            coherent(page);
            page.snapshot.active_tab.as_ref() == Some(tab)
                && page
                    .frame
                    .as_ref()
                    .is_some_and(|f| f.start.tab == *tab && is_blue(centre(f)) == wanted_blue)
        })
        .await;
        coherent(&settled);
    }
    drop(feed);
    world.tasks.shutdown().await.unwrap();
}

/// `current()` of the runtime's execution reader is not an authorization: it
/// is still true after the run is Done, and a graceful Stop request does not
/// change it, while the run itself says it ended or is stopping. Whoever
/// authorizes observation (the service) must ask the run's phase and Stop
/// request itself, as it does.
#[tokio::test(flavor = "multi_thread")]
async fn execution_current_is_not_an_authorization_after_done_or_a_stop_request() {
    struct Answers(tokio::sync::Notify);
    #[async_trait::async_trait]
    impl runtime::AgentModel for Answers {
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            self.0.notified().await;
            Ok(providers::ChatResponse {
                usage_reported: true,
                message: providers::Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
                finish_reason: providers::FinishReason::Stop,
                usage: providers::Usage::default(),
            })
        }
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "observation-lifecycle".into()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("host.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(64));
    let model = Arc::new(Answers(tokio::sync::Notify::new()));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let reader = runtime.browser_execution_reader();

    // a run that answers and is Done
    let done = runtime
        .delegate_chat(
            "thread-done",
            runtime::Role::Worker,
            "go".into(),
            runtime::RunConfig::default(),
        )
        .unwrap();
    let observed = reader.observe(done).expect("in flight");
    assert!(observed.current());
    model.0.notify_one();
    assert_eq!(
        runtime.wait(done).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert_eq!(
        runtime.inspect_agent(done).unwrap().phase,
        event_bus::AgentRunPhase::Done
    );
    assert!(
        observed.current(),
        "Done leaves the incarnation current: that is no permission to observe"
    );
    assert!(reader.observe(done).is_some_and(|now| now.current()));

    // a run with a graceful Stop request
    let stopping = runtime
        .delegate_chat(
            "thread-stop",
            runtime::Role::Worker,
            "go".into(),
            runtime::RunConfig::default(),
        )
        .unwrap();
    let observed = reader.observe(stopping).expect("in flight");
    runtime
        .stop(stopping, runtime::StopScope::SelfOnly)
        .unwrap();
    assert!(runtime.graceful_stop_requested(stopping));
    assert!(
        observed.current(),
        "a Stop request leaves the incarnation current: that is no permission either"
    );
    model.0.notify_one();
    let _ = runtime.wait(stopping).await;
}
