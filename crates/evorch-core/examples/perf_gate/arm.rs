//! One core under test: the real release `evorch-core` of an arm, started the
//! way the SIGKILL matrix starts the sealed product, driven only through the
//! public workbench protocol (CreateExecution / SendChat / subscribe).
use super::{
    fixture,
    gate::{Spec, StateSpec},
    provider::{Mark, Provider},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{Notify, Semaphore},
    task::JoinHandle,
};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_protocol::{
    commands::{Command, ComposerRole, SendChat},
    events::{Subscribe, SubscriptionScope, UiEvent},
    handshake::{BuildInfo, Capability, LocalSurface},
    ids::{ClientId, ProjectId, SubmissionId, ThreadId},
    namespaces::{
        ConversationCommand, ConversationEvent, ExecutionQuery,
        execution::{RunDetailsQuery, RunScope},
    },
    queries::{PageRequest, Query},
    views::MessageRole,
};

pub const READY: Duration = Duration::from_secs(60);
pub const TURN: Duration = Duration::from_secs(60);
pub const TURNS_PER_THREAD: u64 = 20;

fn error(what: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{what:?}"))
}

#[derive(Default)]
struct Counts {
    assistant: HashMap<String, u32>,
    user: HashMap<String, u32>,
    /// Last committed messages per thread, kept only for `run-length` diagnosis.
    tail: HashMap<String, std::collections::VecDeque<String>>,
}
struct Events {
    counts: Mutex<Counts>,
    notify: Notify,
}
impl Events {
    fn assistant(&self, thread: &str) -> u32 {
        self.counts
            .lock()
            .unwrap()
            .assistant
            .get(thread)
            .copied()
            .unwrap_or(0)
    }
    async fn wait_assistant(&self, thread: &str, target: u32, limit: Duration) -> io::Result<()> {
        let deadline = Instant::now() + limit;
        loop {
            let notified = self.notify.notified();
            if self.assistant(thread) >= target {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{thread}: assistant messages stayed at {} below {target}",
                        self.assistant(thread)
                    ),
                ));
            }
        }
    }
}

pub struct CoreArm {
    pub label: &'static str,
    pub provider: Provider,
    root: PathBuf,
    pub commit: String,
    pub pid: u32,
    client: WorkbenchClient,
    events: Arc<Events>,
    created: Mutex<HashSet<String>>,
    seq: AtomicU64,
    probes: AtomicU64,
    child: Mutex<Option<tokio::process::Child>>,
    tasks: Vec<JoinHandle<()>>,
    _dir: test_socket_dir::SplitDir,
}

impl CoreArm {
    pub async fn start(label: &'static str, exe: &Path, spec: &Spec) -> io::Result<Arc<Self>> {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize()?;
        let project = fixture::project_dir(&root);
        std::fs::create_dir(&project)?;
        let state = spec.state.clone();
        let repo = project.clone();
        let commit = tokio::task::spawn_blocking(move || fixture::repository(&repo, &state))
            .await
            .map_err(io::Error::other)??;
        let commands = spec.workloads.iter().map(|w| w.command.clone()).collect();
        let provider =
            Provider::spawn(&project, commands, spec.state.message_bytes as usize).await?;
        fixture::configure(&root, &project, &provider.base_url)?;

        let mut command = tokio::process::Command::new(exe);
        command.env_clear();
        for name in ["PATH", "LD_LIBRARY_PATH", "RUST_LOG"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let log = std::fs::File::create(root.join("core.stderr.log"))?;
        command
            .args(["--config-dir", root.to_str().unwrap(), "--foreground"])
            .env("HOME", &root)
            .env("XDG_STATE_HOME", dir.sockets().join("state"))
            .env("XDG_RUNTIME_DIR", dir.sockets())
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("B3_TEST_KEY", "offline-key")
            .current_dir(&project)
            .stdout(Stdio::piped())
            .stderr(log)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("core has no pid"))?;
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let ready = tokio::time::timeout(READY, lines.next_line())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{label}: core READY")))??
            .ok_or_else(|| io::Error::other(format!("{label}: core exited before READY")))?;
        let ready: Value = serde_json::from_str(&ready).map_err(io::Error::other)?;
        if ready["status"] != "ready" {
            return Err(io::Error::other(format!("{label}: not ready: {ready}")));
        }
        let drain = tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        let mut config = ClientConfig::new(
            ClientId::new("perf-gate").unwrap(),
            BuildInfo {
                name: "perf-gate".into(),
                version: "1".into(),
                revision: None,
            },
        );
        config.required_capabilities.push(Capability::Conversation);
        config.local_surface = Some(LocalSurface::Cli);
        let transport = UnixTransport::connect(ready["socket"].as_str().unwrap())
            .await
            .map_err(error)?;
        let client = WorkbenchClient::connect(transport, config)
            .await
            .map_err(error)?;
        let mut subscription = client
            .subscribe(Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .await
            .map_err(error)?;
        let events = Arc::new(Events {
            counts: Mutex::new(Counts::default()),
            notify: Notify::new(),
        });
        let sink = Arc::clone(&events);
        let trace = std::env::var_os("PERF_GATE_TRACE").is_some();
        let pump = tokio::spawn(async move {
            while let Some(envelope) = subscription.next().await {
                if trace
                    && matches!(
                        envelope.event,
                        UiEvent::Diagnostics(_)
                            | UiEvent::Execution(_)
                            | UiEvent::Approvals(_)
                            | UiEvent::Questions(_)
                    )
                {
                    eprintln!(
                        "[{label}] {}",
                        format!("{:?}", envelope.event)
                            .chars()
                            .take(400)
                            .collect::<String>()
                    );
                }
                if let UiEvent::Conversation(ConversationEvent::MessageCommitted { message }) =
                    envelope.event
                {
                    let mut counts = sink.counts.lock().unwrap();
                    let thread = message.thread.as_str().to_owned();
                    if trace {
                        let text = match &message.content {
                            workbench_protocol::views::MessageContent::Inline { text } => {
                                text.chars().take(240).collect::<String>()
                            }
                            other => format!("{other:?}").chars().take(240).collect(),
                        };
                        let tail = counts.tail.entry(thread.clone()).or_default();
                        tail.push_back(format!("{:?}: {text}", message.role));
                        if tail.len() > 6 {
                            tail.pop_front();
                        }
                    }
                    match message.role {
                        MessageRole::Assistant => *counts.assistant.entry(thread).or_default() += 1,
                        MessageRole::User => *counts.user.entry(thread).or_default() += 1,
                        MessageRole::Tool
                        | MessageRole::Reasoning
                        | MessageRole::Notice
                        | MessageRole::Error
                        | MessageRole::Artifacts => {}
                    }
                    drop(counts);
                    sink.notify.notify_waiters();
                }
            }
        });
        Ok(Arc::new(Self {
            label,
            root,
            provider,
            commit,
            pid,
            client,
            events,
            created: Mutex::new(HashSet::new()),
            seq: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            child: Mutex::new(Some(child)),
            tasks: vec![drain, pump],
            _dir: dir,
        }))
    }

    async fn send(&self, thread: &str, text: String) -> io::Result<()> {
        let first = self.created.lock().unwrap().insert(thread.to_owned());
        let input = SendChat {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new(thread).map_err(error)?,
            submission_id: SubmissionId::new(format!(
                "s{}",
                self.seq.fetch_add(1, Ordering::Relaxed)
            ))
            .map_err(error)?,
            composer_role: ComposerRole::Worker,
            text,
            skills: vec![],
            attachments: vec![],
            model_preference: None,
            fork_seed: None,
        };
        let command = if first {
            ConversationCommand::CreateExecution(input)
        } else {
            ConversationCommand::SendChat(input)
        };
        self.client
            .command(Command::Conversation(command))
            .await
            .map_err(error)?;
        Ok(())
    }

    /// One user message and the whole reply to it: returns once the provider
    /// wrote the closing text and the core committed `assistant` assistant
    /// messages for the thread.
    pub async fn turn(
        &self,
        thread: &str,
        marker: &str,
        text: String,
        assistant: u32,
    ) -> io::Result<Mark> {
        let base = self.events.assistant(thread);
        self.send(thread, text).await?;
        let mark = self.provider.wait_final(marker, TURN).await?;
        self.events
            .wait_assistant(thread, base + assistant, TURN)
            .await?;
        Ok(mark)
    }

    /// A thread holds at most `TURNS_PER_THREAD` probes: the core fails a run after
    /// a few dozen turns, so probes rotate through short-lived threads.
    pub fn probe_thread(&self) -> String {
        format!(
            "probe-{:04}",
            self.probes.fetch_add(1, Ordering::Relaxed) / TURNS_PER_THREAD
        )
    }

    /// The last committed messages of `thread` and the thread's runs as the core reports them.
    pub async fn diagnose(&self, thread: &str) -> String {
        let tail: Vec<String> = self
            .events
            .counts
            .lock()
            .unwrap()
            .tail
            .get(thread)
            .cloned()
            .unwrap_or_default()
            .into();
        let runs = match ThreadId::new(thread) {
            Ok(thread) => self
                .client
                .query(Query::Execution(ExecutionQuery::ListRunDetails(
                    RunDetailsQuery {
                        thread,
                        scope: RunScope::Roots,
                        page: PageRequest::default(),
                    },
                )))
                .await
                .map(|reply| format!("{reply:?}"))
                .unwrap_or_else(|e| format!("query failed: {e:?}")),
            Err(e) => format!("{e:?}"),
        };
        format!("last messages: {tail:#?}\nruns: {runs}")
    }

    pub fn history(&self, thread: &str) -> (u32, u32) {
        let counts = self.events.counts.lock().unwrap();
        (
            counts.user.get(thread).copied().unwrap_or(0),
            counts.assistant.get(thread).copied().unwrap_or(0),
        )
    }

    /// Core process facts, recorded and never judged.
    pub fn observe(&self) -> Value {
        let status =
            std::fs::read_to_string(format!("/proc/{}/status", self.pid)).unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())
        };
        let rollup =
            std::fs::read_to_string(format!("/proc/{}/smaps_rollup", self.pid)).unwrap_or_default();
        let pss = rollup
            .lines()
            .find_map(|l| l.strip_prefix("Pss:"))
            .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok());
        json!({
            "label": self.label,
            "pid": self.pid,
            "rss_anon_kib": field("RssAnon:"),
            "vm_rss_kib": field("VmRSS:"),
            "pss_kib": pss,
            "threads": field("Threads:"),
            "descendants": descendants(self.pid).len(),
            "provider_requests": self.provider.served(),
        })
    }

    pub async fn shutdown(self: Arc<Self>) {
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            // SAFETY: plain kill(2) of the child this arm started.
            unsafe { libc::kill(self.pid as i32, libc::SIGTERM) };
            if tokio::time::timeout(Duration::from_secs(20), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
        if let Some(dir) = std::env::var_os("PERF_GATE_LOG_DIR") {
            let _ = std::fs::copy(
                self.root.join("core.stderr.log"),
                Path::new(&dir).join(format!("{}.stderr.log", self.label)),
            );
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Every live descendant of `root` (from /proc, one scan).
pub fn descendants(root: u32) -> Vec<u32> {
    let mut parents = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if let Some(ppid) = parent_of(pid) {
                parents.insert(pid, ppid);
            }
        }
    }
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &parents {
            if *ppid == parent {
                found.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    found
}

pub fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses: split after the last one.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Builds the loaded state of a core through ordinary conversation commands:
/// `idle_threads` threads of `messages_per_thread` messages of `message_bytes`.
pub async fn populate(arm: &Arc<CoreArm>, spec: &StateSpec) -> io::Result<()> {
    let permits = Arc::new(Semaphore::new(5));
    let mut jobs = Vec::new();
    for t in 0..spec.idle_threads {
        let arm = Arc::clone(arm);
        let permits = Arc::clone(&permits);
        let (turns, bytes) = (spec.messages_per_thread / 2, spec.message_bytes as usize);
        jobs.push(tokio::spawn(async move {
            let _permit = permits.acquire().await.map_err(io::Error::other)?;
            let thread = format!("idle-{t:02}");
            for n in 0..turns {
                let marker = format!("perf:prep:{t}:{n}");
                arm.turn(&thread, &marker, fixture::padded(&marker, bytes), 1)
                    .await?;
            }
            io::Result::Ok(())
        }));
    }
    for job in jobs {
        job.await.map_err(io::Error::other)??;
    }
    for t in 0..spec.idle_threads {
        let (user, assistant) = arm.history(&format!("idle-{t:02}"));
        let want = spec.messages_per_thread / 2;
        if user != want || assistant != want {
            return Err(io::Error::other(format!(
                "{}: idle-{t:02} holds {user} user and {assistant} assistant messages, expected {want} each",
                arm.label
            )));
        }
    }
    Ok(())
}

/// Two agents, each advancing one short read-only tool turn per timer tick.
pub struct Agents {
    stop: Arc<AtomicBool>,
    jobs: Vec<JoinHandle<io::Result<u64>>>,
}
impl Agents {
    pub fn start(arm: &Arc<CoreArm>, spec: &StateSpec) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let jobs = (0..spec.active_agents)
            .map(|k| {
                let (arm, stop) = (Arc::clone(arm), Arc::clone(&stop));
                let period = Duration::from_millis(u64::from(spec.tick_ms));
                tokio::spawn(async move {
                    let mut thread = format!("agent-{k}-0");
                    let mut timer = tokio::time::interval(period);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    let mut ticks = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        timer.tick().await;
                        if ticks > 0 && ticks.is_multiple_of(TURNS_PER_THREAD) {
                            thread = format!("agent-{k}-{}", ticks / TURNS_PER_THREAD);
                        }
                        let marker = format!("perf:tick:{k}:{ticks}");
                        arm.turn(&thread, &marker, format!("{marker} tick"), 1)
                            .await?;
                        ticks += 1;
                    }
                    Ok(ticks)
                })
            })
            .collect();
        Self { stop, jobs }
    }

    /// Stops the timers; an agent that failed fails the round.
    pub async fn stop(self) -> io::Result<Vec<u64>> {
        self.stop.store(true, Ordering::Relaxed);
        let mut ticks = Vec::new();
        for job in self.jobs {
            ticks.push(job.await.map_err(io::Error::other)??);
        }
        Ok(ticks)
    }

    pub fn failed(&self) -> bool {
        self.jobs.iter().any(JoinHandle::is_finished)
    }
}
