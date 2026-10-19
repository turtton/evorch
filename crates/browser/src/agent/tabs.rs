use std::collections::{HashMap, VecDeque};

/// Which run owns each agent tab. Tabs are numbered per run (`t1`, `t2`,
/// …) and never reuse a number. Runs are kept least recently used first.
pub(super) struct Registry<P> {
    runs: VecDeque<Run<P>>,
    unowned: HashMap<String, P>,
    claims: HashMap<String, String>,
    creating: usize,
    started: bool,
    max_pages: usize,
    max_tabs: usize,
}

struct Run<P> {
    key: String,
    tabs: Vec<Tab<P>>,
    active: u32,
    next: u32,
    news: Vec<String>,
}

struct Tab<P> {
    number: u32,
    target: String,
    page: P,
    opener: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Attached {
    /// Give it a tab; otherwise it is closed as soon as Fetch guards it.
    pub keep: bool,
    pub evicted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    pub id: String,
    pub target: String,
    pub active: bool,
    pub opener: Option<String>,
}

impl<P: Clone> Registry<P> {
    pub fn new(max_pages: usize, max_tabs: usize) -> Self {
        Self {
            runs: VecDeque::new(),
            unowned: HashMap::new(),
            claims: HashMap::new(),
            creating: 0,
            started: false,
            max_pages: max_pages.max(1),
            max_tabs: max_tabs.max(1),
        }
    }

    /// Pages that existed before this marks the start stay unowned; later
    /// pages nobody opened or created are closed.
    pub fn start(&mut self) {
        self.started = true;
    }

    /// Notes a `Target.createTarget` in flight, whose page may attach before
    /// its id is known.
    pub fn creating(&mut self, delta: isize) {
        self.creating = self.creating.saturating_add_signed(delta);
    }

    fn tab_limit(&self) -> usize {
        self.max_tabs.min(self.max_pages)
    }

    /// Places a page target that attached. A page another tab opened joins
    /// that tab's run unless the run is at its tab limit.
    pub fn attach(&mut self, target: &str, opener: Option<&str>, page: P) -> Attached {
        let keep = Attached {
            keep: true,
            evicted: Vec::new(),
        };
        let refuse = Attached {
            keep: false,
            evicted: Vec::new(),
        };
        if let Some(key) = self.claims.remove(target) {
            return Attached {
                evicted: self.open_run(key, target, page),
                ..keep
            };
        }
        let Some(opener) = opener else {
            if self.started && self.creating == 0 {
                return refuse;
            }
            self.unowned.insert(target.to_owned(), page);
            return keep;
        };
        let limit = self.tab_limit();
        let Some(index) = self.run_of(opener) else {
            return refuse;
        };
        let mut run = self.runs.remove(index).expect("index from run_of");
        let from = run.number_of(opener);
        let opened = if run.tabs.len() >= limit {
            run.news.push(format!(
                "a tab {from} opened was closed: this run already has {limit} tabs; browser_close_tab frees one"
            ));
            false
        } else {
            let number = run.next;
            run.next += 1;
            run.tabs.push(Tab {
                number,
                target: target.to_owned(),
                page,
                opener: run
                    .tabs
                    .iter()
                    .find(|tab| tab.target == opener)
                    .map(|tab| tab.number),
            });
            run.news.push(format!(
                "t{number} opened from {from}; browser_switch_tab moves to it"
            ));
            true
        };
        let key = run.key.clone();
        self.runs.push_back(run);
        Attached {
            keep: opened,
            evicted: if opened { self.evict(&key) } else { Vec::new() },
        }
    }

    pub fn claim(&mut self, key: &str, target: &str) -> Vec<String> {
        match self.unowned.remove(target) {
            Some(page) => self.open_run(key.to_owned(), target, page),
            None => {
                self.claims.insert(target.to_owned(), key.to_owned());
                Vec::new()
            }
        }
    }

    pub fn claimed(&self, target: &str) -> bool {
        self.claims.contains_key(target)
    }

    fn open_run(&mut self, key: String, target: &str, page: P) -> Vec<String> {
        let mut evicted = self.remove_run(&key);
        self.runs.push_back(Run {
            key: key.clone(),
            tabs: vec![Tab {
                number: 1,
                target: target.to_owned(),
                page,
                opener: None,
            }],
            active: 1,
            next: 2,
            news: Vec::new(),
        });
        evicted.extend(self.evict(&key));
        evicted
    }

    /// Closes whole runs, least recently used first, until every run's tabs
    /// together fit `max_pages`; `keep` is never closed.
    fn evict(&mut self, keep: &str) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.runs.iter().map(|run| run.tabs.len()).sum::<usize>() > self.max_pages {
            let Some(index) = self.runs.iter().position(|run| run.key != keep) else {
                break;
            };
            let run = self.runs.remove(index).expect("index from position");
            evicted.extend(run.tabs.into_iter().map(|tab| tab.target));
        }
        evicted
    }

    fn run_of(&self, target: &str) -> Option<usize> {
        self.runs
            .iter()
            .position(|run| run.tabs.iter().any(|tab| tab.target == target))
    }

    fn run(&self, key: &str) -> Option<&Run<P>> {
        self.runs.iter().find(|run| run.key == key)
    }

    fn run_mut(&mut self, key: &str) -> Option<&mut Run<P>> {
        self.runs.iter_mut().find(|run| run.key == key)
    }

    pub fn touch(&mut self, key: &str) {
        if let Some(index) = self.runs.iter().position(|run| run.key == key)
            && let Some(run) = self.runs.remove(index)
        {
            self.runs.push_back(run);
        }
    }

    pub fn active(&self, key: &str) -> Option<(String, P)> {
        let run = self.run(key)?;
        run.tabs
            .iter()
            .find(|tab| tab.number == run.active)
            .map(|tab| (format!("t{}", tab.number), tab.page.clone()))
    }

    pub fn tabs(&self, key: &str) -> Vec<Listed> {
        self.run(key)
            .map(|run| {
                run.tabs
                    .iter()
                    .map(|tab| Listed {
                        id: format!("t{}", tab.number),
                        target: tab.target.clone(),
                        active: tab.number == run.active,
                        opener: tab.opener.map(|number| format!("t{number}")),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn switch(&mut self, key: &str, id: &str) -> Result<P, String> {
        let run = self
            .run_mut(key)
            .ok_or("this run has no open browser tab")?;
        let tab = run.find(id)?;
        let (number, page) = (tab.number, tab.page.clone());
        run.active = number;
        Ok(page)
    }

    /// Forgets tab `id` and returns its target to close. Closing the last
    /// tab ends the run's page like `browser_close`.
    pub fn close_tab(&mut self, key: &str, id: &str) -> Result<String, String> {
        let index = self
            .runs
            .iter()
            .position(|run| run.key == key)
            .ok_or("this run has no open browser tab")?;
        let number = self.runs[index].find(id)?.number;
        let target = self.runs[index].remove(number);
        if self.runs[index].tabs.is_empty() {
            self.runs.remove(index);
        }
        Ok(target)
    }

    /// Forgets a target that went away on its own, such as a popup that
    /// closed itself. Returns whether it was known.
    pub fn detached(&mut self, target: &str) -> bool {
        self.claims.remove(target);
        if self.unowned.remove(target).is_some() {
            return true;
        }
        let Some(index) = self.run_of(target) else {
            return false;
        };
        let run = &mut self.runs[index];
        let number = run.number_of(target);
        let was_active = run
            .tabs
            .iter()
            .any(|tab| tab.target == target && tab.number == run.active);
        run.remove_target(target);
        if run.tabs.is_empty() {
            self.runs.remove(index);
        } else {
            let now = if was_active {
                format!("; t{} is now active", run.active)
            } else {
                String::new()
            };
            run.news.push(format!("{number} closed{now}"));
        }
        true
    }

    pub fn remove_run(&mut self, key: &str) -> Vec<String> {
        match self.runs.iter().position(|run| run.key == key) {
            Some(index) => self
                .runs
                .remove(index)
                .into_iter()
                .flat_map(|run| run.tabs.into_iter().map(|tab| tab.target))
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.runs.iter().map(|run| run.key.clone()).collect();
        keys.sort();
        keys
    }

    pub fn take_news(&mut self, key: &str) -> Vec<String> {
        self.run_mut(key)
            .map(|run| std::mem::take(&mut run.news))
            .unwrap_or_default()
    }
}

impl<P> Run<P> {
    fn number_of(&self, target: &str) -> String {
        self.tabs
            .iter()
            .find(|tab| tab.target == target)
            .map_or_else(|| "a closed tab".into(), |tab| format!("t{}", tab.number))
    }

    fn find(&self, id: &str) -> Result<&Tab<P>, String> {
        id.strip_prefix('t')
            .and_then(|number| number.parse::<u32>().ok())
            .and_then(|number| self.tabs.iter().find(|tab| tab.number == number))
            .ok_or_else(|| {
                let open: Vec<String> = self
                    .tabs
                    .iter()
                    .map(|tab| format!("t{}", tab.number))
                    .collect();
                format!("no tab {id}; open tabs: {}", open.join(", "))
            })
    }

    fn remove(&mut self, number: u32) -> String {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.number == number)
            .expect("number of an open tab");
        let tab = self.tabs.remove(index);
        if self.active == number {
            self.active = tab
                .opener
                .filter(|opener| self.tabs.iter().any(|tab| tab.number == *opener))
                .or_else(|| self.tabs.last().map(|tab| tab.number))
                .unwrap_or(0);
        }
        tab.target
    }

    fn remove_target(&mut self, target: &str) {
        if let Some(number) = self
            .tabs
            .iter()
            .find(|tab| tab.target == target)
            .map(|tab| tab.number)
        {
            self.remove(number);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(max_pages: usize, max_tabs: usize) -> Registry<&'static str> {
        let mut registry = Registry::new(max_pages, max_tabs);
        registry.start();
        registry
    }

    fn create(
        registry: &mut Registry<&'static str>,
        key: &str,
        target: &'static str,
    ) -> Vec<String> {
        registry.creating(1);
        let attached = registry.attach(target, None, target);
        assert!(attached.keep);
        let evicted = registry.claim(key, target);
        registry.creating(-1);
        evicted
    }

    fn ids(registry: &Registry<&'static str>, key: &str) -> Vec<String> {
        registry
            .tabs(key)
            .into_iter()
            .map(|tab| format!("{}{}", tab.id, if tab.active { "*" } else { "" }))
            .collect()
    }

    #[test]
    fn popups_join_their_openers_run_up_to_its_tab_limit() {
        // Given: a run with one tab and a limit of two tabs per run.
        let mut registry = registry(8, 2);
        create(&mut registry, "run-1", "A");

        // When: its page opens two popups.
        let first = registry.attach("B", Some("A"), "B");
        let second = registry.attach("C", Some("A"), "C");

        // Then: the first joins as t2 behind the active t1, the second is
        // closed, and both are reported once.
        assert!(first.keep);
        assert!(!second.keep);
        assert_eq!(ids(&registry, "run-1"), ["t1*", "t2"]);
        let news = registry.take_news("run-1");
        assert_eq!(news.len(), 2, "{news:?}");
        assert!(news[0].starts_with("t2 opened from t1"), "{news:?}");
        assert!(news[1].contains("already has 2 tabs"), "{news:?}");
        assert!(registry.take_news("run-1").is_empty());
    }

    #[test]
    fn pages_nobody_opened_are_refused_once_started() {
        // Given: one page that existed before the start and one run.
        let mut registry = Registry::new(8, 4);
        assert!(registry.attach("initial", None, "initial").keep);
        registry.start();
        create(&mut registry, "run-1", "A");

        // When: pages appear with no opener, or an opener no run owns.
        let orphan = registry.attach("X", None, "X");
        let stray = registry.attach("Y", Some("initial"), "Y");

        // Then: neither gets a place, so both are closed.
        assert!(!orphan.keep);
        assert!(!stray.keep);
        assert_eq!(registry.keys(), ["run-1"]);
    }

    #[test]
    fn a_created_page_that_attaches_after_its_claim_still_opens_the_run() {
        let mut registry = registry(8, 4);
        registry.creating(1);

        registry.claim("run-1", "A");
        assert!(registry.claimed("A"));
        let attached = registry.attach("A", None, "A");
        registry.creating(-1);

        assert!(attached.keep);
        assert!(!registry.claimed("A"));
        assert_eq!(registry.active("run-1"), Some(("t1".into(), "A")));
    }

    #[test]
    fn going_over_max_pages_closes_the_least_recently_used_other_run() {
        // Given: three tabs allowed in all, held by two runs.
        let mut registry = registry(3, 3);
        create(&mut registry, "run-1", "A");
        create(&mut registry, "run-2", "B");
        registry.attach("C", Some("B"), "C");
        registry.touch("run-1");

        // When: run-2 opens another popup after run-1 was used.
        let attached = registry.attach("D", Some("C"), "D");

        // Then: run-1 is closed, even though it was used more recently,
        // because run-2 is the run that is acting.
        assert!(attached.keep);
        assert_eq!(attached.evicted, ["A"]);
        assert_eq!(registry.keys(), ["run-2"]);
        assert_eq!(ids(&registry, "run-2"), ["t1*", "t2", "t3"]);
    }

    #[test]
    fn the_tab_limit_never_exceeds_max_pages() {
        let mut registry = registry(2, 4);
        create(&mut registry, "run-1", "A");
        assert!(registry.attach("B", Some("A"), "B").keep);

        assert!(!registry.attach("C", Some("B"), "C").keep);
    }

    #[test]
    fn closing_the_active_tab_returns_to_its_opener_and_the_last_tab_ends_the_run() {
        // Given: t1 opened t2, which opened t3, and t3 is active.
        let mut registry = registry(8, 4);
        create(&mut registry, "run-1", "A");
        registry.attach("B", Some("A"), "B");
        registry.attach("C", Some("B"), "C");
        assert_eq!(registry.switch("run-1", "t3"), Ok("C"));

        // When: the agent closes t3, the page closes t2, then t1 is closed.
        let closed = registry.close_tab("run-1", "t3");
        let after_close = ids(&registry, "run-1");
        assert!(registry.detached("B"));
        let after_detach = ids(&registry, "run-1");
        let news = registry.take_news("run-1");
        let missing = registry.switch("run-1", "t9");
        let last = registry.close_tab("run-1", "t1");

        // Then: the opener became active each time and the run ended.
        assert_eq!(closed, Ok("C".to_owned()));
        assert_eq!(after_close, ["t1", "t2*"]);
        assert_eq!(after_detach, ["t1*"]);
        assert!(
            news.iter()
                .any(|entry| entry == "t2 closed; t1 is now active")
        );
        assert_eq!(missing, Err("no tab t9; open tabs: t1".into()));
        assert_eq!(last, Ok("A".to_owned()));
        assert!(registry.keys().is_empty());
        assert!(!registry.detached("A"));
    }

    #[test]
    fn reopening_a_run_replaces_its_old_tabs() {
        let mut registry = registry(8, 4);
        create(&mut registry, "run-1", "A");

        let evicted = create(&mut registry, "run-1", "B");

        assert_eq!(evicted, ["A"]);
        assert_eq!(registry.active("run-1"), Some(("t1".into(), "B")));
    }
}
